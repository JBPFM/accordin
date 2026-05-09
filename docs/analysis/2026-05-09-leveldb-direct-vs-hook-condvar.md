# LevelDB direct 模式和 hook-all condvar 路径差异

日期：2026-05-09

这份文档解释 `mcs_tas_accordin_direct` 和 `mutex_hook` hook-all 在
LevelDB `fillrandom` 下的真实差异。重点是澄清一个容易混淆的问题：

**direct 模式下 `DBImpl::mutex_` 仍然用在 condition variable 语义里。**
但是 direct 没有接管 condition variable 的底层等待队列；hook-all 则接管了
pthread mutex 和 pthread condvar 两层。

所以 direct 和 hook-all 的差异不是“`DBImpl::mutex_` 有没有参与 condvar”，
而是：

```text
direct:
  native condvar park/wake queue
  + direct MCS-TAS DBImpl::mutex_ unlock/relock

hook-all:
  Accordin pthread_cond_wait implementation
  + Accordin pthread_mutex_lock/unlock implementation
  + 进程内其他 pthread mutex/condvar 也被同一个 hook 层接管
```

## 结论

`direct` 模式只替换 LevelDB 显式 opt-in 的 `DBImpl::mutex_`。非队首 writer
在等待 writer queue 轮到自己时，仍然由 libstdc++/glibc 的 condition
variable 机制 park。Accordin 只在释放和重新获取 `DBImpl::mutex_` 的时候参与。

`mutex_hook` hook-all 模式通过 `LD_PRELOAD=libmcs_tas_accordin.so` 接管
pthread 同步 API。LevelDB 的 `std::mutex` / `std::condition_variable` 最终也会
走到这些 pthread symbols，因此 DB 主锁、condvar wait、condvar wake、以及
运行时/库里的其他 pthread 同步对象都会进入 Accordin hook 路径。

因此，前面简单说“spinlock 不适合 condvar 场景”是不够准确的。更准确的说法是：

**短热点 DB mutex 可以用 MCS-TAS/direct 获益；但是把 condvar wait queue 和全进程
pthread 同步层也一起替换成 Accordin hook，会把每次 writer handoff 的成本放大。**

## direct 模式的代码路径

LevelDB 在 `DBImpl` 构造函数中只对 `mutex_` 做 direct opt-in：

- `third_party/leveldb-1.23/db/db_impl.cc`
  - `DBImpl::DBImpl(...)` 调用 `mutex_.UseMcsTasAccordinDirect()`
  - 同一个 `mutex_` 保护 writer queue、sequence、memtable/version 等 DB 状态

`port::Mutex` 的 direct dispatch 在：

- `third_party/leveldb-1.23/port/port_stdcxx.h`
  - `Mutex::UseMcsTasAccordinDirect()` 创建 `mcs_tas_accordin_direct_mutex`
  - `Mutex::Lock()` 调 `mcs_tas_accordin_direct_mutex_lock()`
  - `Mutex::Unlock()` 调 `mcs_tas_accordin_direct_mutex_unlock()`

direct preload 库导出的符号是 direct API：

```text
mcs_tas_accordin_direct_mutex_create
mcs_tas_accordin_direct_mutex_destroy
mcs_tas_accordin_direct_mutex_lock
mcs_tas_accordin_direct_mutex_trylock
mcs_tas_accordin_direct_mutex_unlock
```

它不导出：

```text
pthread_cond_wait
pthread_cond_timedwait
pthread_mutex_lock
pthread_mutex_unlock
```

所以 direct 模式不会 interpose libstdc++/glibc 的 condvar 内部实现。

direct 下 `CondVar::Wait()` 的 LevelDB 代码是：

```cpp
void Wait() {
  if (mu_->UsesMcsTasAccordinDirect()) {
    std::unique_lock<Mutex> lock(*mu_, std::adopt_lock);
    cv_any_.wait(lock);
    lock.release();
  } else {
    std::unique_lock<std::mutex> lock(mu_->mu_, std::adopt_lock);
    cv_.wait(lock);
    lock.release();
  }
}
```

这里 `mu_` 就是 `DBImpl::mutex_`。所以 direct 下 `DBImpl::mutex_` 仍然是 condvar
的 user lock：`Wait()` 会释放它，睡眠，醒来后重新获取它。

但 `std::condition_variable_any` 的底层等待队列不是 Accordin 实现的。libstdc++
的实现大致是：

```text
condition_variable_any::wait(user_lock):
  lock internal std::mutex
  unlock user_lock        # DBImpl::mutex_.Unlock(), direct MCS-TAS
  native condition_variable wait on internal std::mutex
  relock user_lock        # DBImpl::mutex_.Lock(), direct MCS-TAS
```

因此 direct 下非队首 writer 在等待时是 native park，不是在 Accordin spinlock
上一直转。Accordin 只负责 DB 主锁真正 unlock/relock 的边界。

## hook-all 模式的代码路径

hook-all 模式使用：

```text
LD_PRELOAD=target/release/libmcs_tas_accordin.so
```

这个库导出 pthread interpose symbols：

```text
pthread_cond_wait
pthread_cond_timedwait
pthread_cond_signal
pthread_cond_broadcast
pthread_mutex_lock
pthread_mutex_unlock
pthread_mutex_trylock
pthread_mutex_init
pthread_mutex_destroy
```

因为这个库不导出 direct API，`UseMcsTasAccordinDirect()` 不会成功切换到 direct
backend。LevelDB 会落回普通 `std::mutex` / `std::condition_variable` 分支。
但是这些 pthread 调用会被 preload 库 interpose。

hook-all 的核心实现位于：

- `src/accordin_shared/src/mutex_hook.rs`
  - `pthread_cond_wait()`
  - `pthread_cond_timedwait()`
  - `pthread_mutex_lock()`
  - `pthread_mutex_unlock()`

原始 hook-all condvar wait 路径是：

```text
pthread_cond_wait(cond, user_mutex):
  ensure_state(user_mutex)
  ensure_cond_state(cond)
  waiters++
  unlock_with_stats(user_mutex)
  futex wait on Accordin cond seq
  lock_with_stats(user_mutex)
```

其中 `unlock_with_stats()` / `lock_with_stats()` 不只是 raw lock/unlock。它们还会做：

- lock statistics sampling
- wait/hold/outside gap 采样
- admission scope begin/finish
- slow-path pending 标记
- 可能的 `yield_now()`
- raw MCS-TAS lock/unlock
- post-unlock dynamic control 检查

2026-05-09 的实验中，我测试了一个 lighter condvar path：condvar 内部 release/reacquire
不再走完整 stats 链，而是：

```text
pthread_cond_wait(cond, user_mutex):
  ensure_state(user_mutex)
  ensure_cond_state(cond)
  waiters++
  unlock_for_cond_wait(user_mutex)
  futex wait on Accordin cond seq
  lock_for_cond_wait(user_mutex)
```

保留 slow-path admission、去掉 stats 链后，`fillrandom` 有改善；但仍然离 direct/FlexGuard
很远。完全去掉 condvar reacquire 的 slow-path admission 反而更差。这说明瓶颈不只是
stats，而是 hook-all 把 condvar wait/reacquire 也放进 Accordin pthread hook 边界后，
writer handoff 的整体成本太高。

## LevelDB fillrandom 的同步路径

`fillrandom` 的核心写路径是 `DBImpl::Write()`：

```text
Writer w(&mutex_)
MutexLock l(&mutex_)
writers_.push_back(&w)

while (!w.done && &w != writers_.front()):
  w.cv.Wait()

MakeRoomForWrite(...)
BuildBatchGroup(...)

mutex_.Unlock()
  log_->AddRecord(...)
  WriteBatchInternal::InsertInto(...)
mutex_.Lock()

pop ready writers
ready->cv.Signal()
writers_.front()->cv.Signal()
```

几个关键点：

- writer queue 由 `DBImpl::mutex_` 保护。
- 非队首 writer 会在 per-writer condvar 上等待。
- 真正较重的 WAL append 和 memtable insert 在 `mutex_.Unlock()` 之后执行。
- 锁内主要是 writer queue、sequence、状态更新等短临界区。
- 128 线程下 writer handoff 频率很高。

这解释了 direct 为什么可以表现好：

```text
短热点 DB mutex:
  用 MCS-TAS/direct 替换可能降低 mutex lock/unlock 成本

writer 等待队列:
  仍然由 native condvar park/wake 管理
```

也解释了 hook-all 为什么差：

```text
高频 writer handoff:
  每次 wait/reacquire 都进入 Accordin pthread_cond_wait + pthread_mutex hook

全进程 interpose:
  无关的 std::mutex / pthread mutex / condvar 也可能进入 Accordin hook 层
```

## 128 线程数据

2026-05-09 的一次 fresh run：

| 模式 | benchmark | latency us/op | ops/s | wall seconds |
| --- | --- | ---: | ---: | ---: |
| FlexGuard | fillrandom | 420.072 | 304,709.669 | 5.334 |
| direct | fillrandom | 1360.678 | 94,070.750 | 17.110 |
| hook-all original | fillrandom | 5186.362 | 24,680.113 | 64.753 |
| hook-all cond raw, no admission | fillrandom | 6369.721 | 20,095.072 | 79.420 |
| hook-all cond raw, admission kept | fillrandom | 3738.262 | 34,240.511 | 46.532 |

这个 fresh run 中 direct 并没有超过 FlexGuard，但显著好于 hook-all。核心差异不是
direct 没有用 spinlock，而是 direct 的 spinlock 边界更窄：只包住 DB 主锁的
unlock/relock，不接管 condvar wait queue 和全进程 pthread 同步层。

perf stat 也支持这个判断：

| 模式 | cycles/op | instructions/op | context switches/op | migrations/op |
| --- | ---: | ---: | ---: | ---: |
| FlexGuard | 728,733 | 108,724 | 0.158 | 0.035 |
| direct | 545,609 | 72,857 | 2.039 | 1.851 |
| hook-all original | 7,642,794 | 600,183 | 1.535 | 0.311 |

hook-all 的 per-op CPU work 放大了一个数量级。direct 有更多 context switch/migration，
说明它更多依赖 native park/wake，而不是一直在用户态 hook/spin 链里烧 CPU。

## 为什么这不和“direct 也是 spinlock”矛盾

`direct` 确实把 `DBImpl::mutex_` 替换成了 MCS-TAS spin/admission lock。

但是非队首 writer 等待 condvar 时不是在这个 spinlock 上一直 spin。direct 下等待流程是：

```text
writer holds DBImpl::mutex_
  -> condition_variable_any locks internal native mutex
  -> releases DBImpl::mutex_ through direct MCS-TAS unlock
  -> parks on native condition_variable
  -> after signal, reacquires DBImpl::mutex_ through direct MCS-TAS lock
```

所以 direct 中 Accordin 只参与“释放/重取 DB mutex”，不参与“condvar 等待队列本身”。

hook-all 则是：

```text
writer holds std::mutex-backed DBImpl::mutex_
  -> pthread_cond_wait is interposed
  -> Accordin cond state / waiter seq / futex path
  -> Accordin unlock/relock path
  -> admission/scope/stats or lighter condwait helper
```

所以 hook-all 把 condvar 等待和 mutex reacquire 都放进 Accordin 的 pthread hook
实现里了。

更准确的结论是：

```text
一个短热点 DB mutex 用 direct MCS-TAS 可能有收益；
把 condvar 和全进程 pthread 同步层全部 hook 成 Accordin，LevelDB fillrandom 会变差。
```

## 对后续实现的启发

对 LevelDB experiment four：

- direct 模式适合评估“只替换 DB 主热点锁”的效果。
- hook-all 不适合作为 LevelDB `fillrandom` 的默认性能路径，因为它改变了太多同步对象。
- 如果要继续优化 hook 模式，应该避免把 condvar wait queue 也接管进 Accordin。
- 一个更合理的目标是：`native condvar wait queue + Accordin user mutex unlock/relock`。
  direct 通过 `condition_variable_any` 实际上就是这种形态。
- 另一个方向是给 Accordin condvar reacquire 增加类似 FlexGuard 的 blocking/parking phase，
  避免 128 writer 醒来后在 MCS-TAS/TAS front 上产生过多用户态竞争。

## 相关结果路径

```text
experiments/results/experiment4_flexguard_128_compare_20260509_214600
experiments/results/experiment4_direct_fillrandom_128_compare_20260509_213607
experiments/results/experiment4_hook_all_128_retry_20260509_212409
experiments/results/experiment4_hook_all_condraw_128_20260509_215055
experiments/results/experiment4_hook_all_condraw_admission_128_20260509_215509
experiments/results/analysis_hook_all_fillrandom_128_perf
```

## 复现实验命令

```bash
python3 experiments/run_experiment_four.py \
  --accordin-mode accordin_direct \
  --locks accordin \
  --benchmarks fillrandom \
  --threads 128 \
  --repeats 1

python3 experiments/run_experiment_four.py \
  --accordin-mode mutex_hook \
  --locks accordin \
  --benchmarks fillrandom \
  --threads 128 \
  --repeats 1

python3 experiments/run_experiment_four.py \
  --locks flexguard \
  --benchmarks fillrandom \
  --threads 128 \
  --repeats 1
```

