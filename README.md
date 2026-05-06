# Accordin

Accordin 是一个通过 `LD_PRELOAD` 接入 `pthread_mutex` 的用户态锁与 eBPF `sched_ext` 调度器组合。当前仓库可以按职责分成三部分：

1. **concurrency controller**：通过 CS/NCS 估计并动态调整有效并发度。
2. **admission yield**：线程进入锁等待前与 BPF 协作，抑制等待者过载。
3. **mutex hook**：把多个锁实现封装成 `pthread_mutex` ABI，供现有程序透明接入。

## 1. Concurrency Controller

concurrency controller 主要在 `src/accordin_shared/src/lock_stats.rs` 和 `src/accordin_shared/src/cpu_affinity.rs` 中实现。

它采样锁操作并统计三类时间：

- **CS**：critical section，线程持锁执行的时间，即从加锁成功到解锁。
- **NCS**：non-critical section，线程两次持锁之间在锁外执行的时间，采样时会扣除等待锁的时间。
- **wait**：竞争锁时的等待时间。

启用 CPU affinity 控制后，controller 周期性使用平均 CS/NCS 计算目标并发度：

```text
target_concurrency = 1 + avg_ncs / avg_cs
```

计算结果会经过 EWMA 平滑，当前 `alpha = 0.2`，然后四舍五入成 CPU 数。若新旧 CPU 数只差 1，则保持不变，避免频繁抖动。最终通过 `sched_setaffinity` 更新进程内线程可运行的 CPU 集合。

关键入口：

- `record_lock_acquired()`：加锁成功后开始 CS 计时。
- `record_post_unlock()`：解锁后结束 CS 计时，并开启下一段 NCS 采样窗口。
- `run_dynamic_cpu_control_tick()`：根据 CS/NCS 样本计算目标并发度。
- `cpu_affinity::update_dynamic_cpu_count()`：把目标并发度应用为 CPU mask。

进程退出时会输出聚合统计：

```text
stats_label: accordin
avg_critical_ns: ...
avg_outside_ns: ...
avg_outside_ns_elapsed: ...
outside_ns_unlock_gap_samples: ...
dynamic_cpu_affinity_cpus: ...
```

## 2. Admission Yield

admission yield 是用户态锁与 BPF 调度器之间的握手机制。目标是在大量线程即将进入锁慢路径等待时，让 BPF 控制哪些等待者可以继续活跃运行，避免所有等待者同时消耗 CPU。

用户态状态在 `src/accordin_shared/src/admission.rs`，BPF 侧调度逻辑在 `src/bpf/main.bpf.c`。

每个线程有一个 admission word，当前使用三个 bit：

- `SLOW_PATH_PENDING`：锁 fast path 失败，线程即将进入慢路径等待。
- `IN_CRITICAL_SECTION`：线程已经获得锁，正在临界区中执行。
- `SLOW_PATH_SEEN`：线程生命周期内曾经进入过锁慢路径。

线程第一次经过 mutex hook 时，会把自己的 TID 与 admission word 地址注册到 BPF 的 `thread_ctx_addr_map`。之后 BPF 可以通过 `bpf_probe_read_user()` 读取用户态 admission word，并据此做 admission 决策。

基本流程：

1. mutex hook 的 fast path 失败后，由 shared adapter 在进入等待前调用 `mark_slow_path_pending()`，同时设置 `SLOW_PATH_PENDING` 和持久的 `SLOW_PATH_SEEN`。
2. BPF 看到 `SLOW_PATH_PENDING` 后，为该任务选择 admission CPU。
3. `cpu_admission_owner_map` 对每个 CPU 只记录一个当前 admission owner。
4. 拿不到 admission 的等待者会被放入该 CPU 的 inactive DSQ。
5. 线程获得锁后设置 `IN_CRITICAL_SECTION`，并清除 `SLOW_PATH_PENDING`，但保留 `SLOW_PATH_SEEN`。
6. 线程解锁后清除 `IN_CRITICAL_SECTION`；BPF 释放对应 CPU 的 admission owner，但保留该线程的 slow-path history。
7. 之后只要 `SLOW_PATH_SEEN` 已经存在，该线程即使在锁外运行，也会进入 controller 的 active CPU 范围，通过 controlled DSQ 只由 active CPU 消费。
8. 未进入过慢路径的普通任务使用内建 `SCX_DSQ_GLOBAL` / local DSQ；如果普通任务的 affinity 只允许一个 CPU，则 enqueue 直接投到该 CPU 的 local DSQ，避免被 controlled work 饥饿。

关键 BPF 路径：

- `accordin_select_cpu()`：让已 admission 的任务继续回到 admission CPU。
- `accordin_enqueue()`：为慢路径等待者授予 admission，或放入 inactive DSQ；对 `SLOW_PATH_SEEN` 线程投递 controlled DSQ；普通任务投递内建 global/local DSQ。
- `accordin_dispatch()`：内建 local/global DSQ 已经为空时才被调用；active CPU 周期性调度 inactive waiters，并消费 controlled DSQ；inactive CPU 不消费 controlled/inactive work。
- `accordin_stopping()`：若 runnable 任务仍在 CS 中，则保护其 admission。
- `accordin_exit_task()`：清理 task storage 与 thread-context map。

## 3. Mutex Hook

mutex hook 在 `src/accordin_shared/src/mutex_hook.rs` 中实现。它通过 `export_mutex_hooks!` 宏导出 `pthread_mutex_*` 和 `pthread_cond_*` 符号，使应用无需改源码即可把 `pthread_mutex` 调用接入 Accordin 的锁后端。

hook 层负责：

- 在 `pthread_mutex_t` 存储中懒加载每个 mutex 对应的 backend state。
- 导出 `pthread_mutex_init`、`pthread_mutex_destroy`、`pthread_mutex_lock`、`pthread_mutex_trylock`、`pthread_mutex_unlock`。
- 基于 futex 提供基础 `pthread_cond_*` 支持。
- 在线程首次进入 hook 时注册 admission word 到 BPF。
- 在 backend lock/unlock 周围记录 wait/CS/NCS 统计。

后端通过下面的 trait 接入：

```rust
pub trait MutexHookBackend {
    type LockState;

    fn create_state() -> Self::LockState;
    fn lock(state: &Self::LockState);
    fn try_lock(state: &Self::LockState) -> bool;
    fn unlock(state: &Self::LockState);
}
```

当前可 preload 的库：

| Library | Backend | BPF admission | 位置 |
| --- | --- | --- | --- |
| `libaccordin.so` | MCS-TAS | yes | 根 crate 默认实现 |
| `libmcs_accordin.so` | MCS | yes | `src/mcs_accordin` |
| `libmcs_tas_accordin.so` | MCS-TAS | yes | `src/mcs_tas_accordin` |
| `libttas_accordin.so` | TTAS | yes | `src/ttas_accordin` |
| `libreciprocating_accordin.so` | Reciprocating | yes | `src/reciprocating_accordin` |
| `libmcs_tse.so` | MCS + timeslice extension | no | `src/mcs_tse` |

新增普通锁实现时，通常需要：

1. 实现原始锁状态，并为其实现 `LockBackend`。
2. 为 raw lock 实现 `Default`，然后用 `accordin_shared::mutex_hook::LockBackendAdapter<RawLock>` 接入 hook。
3. 用 `accordin_shared::export_mutex_hooks!(...)` 导出 pthread hook。
4. 如果需要独立 preload，则把新 crate 加入 workspace，并设置 `crate-type = ["cdylib"]`。
5. raw lock 文件只保留锁算法；slow-path admission、CS 状态、统计和动态并发控制由 `accordin_shared` 的 hook/stats 层处理。需要额外 hook-local 状态的后端仍可直接实现 `MutexHookBackend`。

## 运行数据流

```text
LD_PRELOAD 加载动态库
  -> .init_array 初始化日志、CPU affinity 和 BPF
  -> Rust 侧把 thread_ctx_addr_map 传给 mutex hook
  -> 第一次 pthread mutex 操作注册当前线程 admission word
  -> try_lock fast path 成功则直接进入 CS
  -> fast path 失败则在慢路径前设置 SLOW_PATH_PENDING
  -> BPF 根据 admission word 控制等待者是否进入 active 调度，并记录 SLOW_PATH_SEEN
  -> 加锁成功后设置 IN_CRITICAL_SECTION 并开始 CS 统计
  -> 解锁时清除 CS 状态，保留 slow-path history，更新统计，并可能调整 CPU 并发度
  -> 曾经进入过慢路径的线程继续通过 controlled DSQ 限制在 active CPUs 上运行
  -> .fini_array 打印进程级锁统计
```

## 构建

构建全部 preload 库：

```sh
cargo build --release
```

只构建某一个后端：

```sh
cargo build --release -p mcs_accordin
```

BPF admission 依赖 Linux `sched_ext`。运行带 BPF 的 preload 库通常需要 root 或等价的 BPF 权限。

## 运行

对现有 pthread 程序使用默认 Accordin 后端：

```sh
sudo K=2 LD_PRELOAD=./target/release/libaccordin.so ./your_program
```

选择具体后端：

```sh
sudo K=4 LD_PRELOAD=./target/release/libmcs_accordin.so ./your_program
sudo K=4 LD_PRELOAD=./target/release/libttas_accordin.so ./your_program
sudo K=4 LD_PRELOAD=./target/release/libreciprocating_accordin.so ./your_program
```

`K` 是 `ACCORDIN_CPU_MASK_K` 的短写，表示初始启用的 CPU 数。CPU 集合来自第一个 NUMA node 中当前进程可用的 CPU。启用后，concurrency controller 可以根据 CS/NCS 样本动态调整 active CPU 数。

## 环境变量

| 变量 | 含义 |
| --- | --- |
| `ACCORDIN_CPU_MASK_K` / `K` | 启用 CPU affinity 控制，并设置初始 active CPU 数。 |
| `ACCORDIN_SAMPLE_STRIDE` | 锁统计采样步长，默认 `8`。 |
| `ACCORDIN_DYNAMIC_CPU_WINDOW_NS` | 动态并发控制窗口，默认 `1000000` ns。 |
| `ACCORDIN_DISABLE_BPF` | 对根 crate `libaccordin.so` 禁用 BPF。 |
| `MCS_ACCORDIN_DISABLE_BPF` | 对 `libmcs_accordin.so` 禁用 BPF。 |
| `MCS_TAS_ACCORDIN_DISABLE_BPF` | 对 `libmcs_tas_accordin.so` 禁用 BPF。 |
| `TTAS_ACCORDIN_DISABLE_BPF` | 对 `libttas_accordin.so` 禁用 BPF。 |
| `RECIPROCATING_ACCORDIN_DISABLE_BPF` | 对 `libreciprocating_accordin.so` 禁用 BPF。 |

`*_STATS_ONLY` 和 `*_DEBUG_COUNTERS` 变量会被对应 preload 库解析，但当前 minimal BPF controller 会报告这些模式被忽略。

## 源码索引

| 路径 | 职责 |
| --- | --- |
| `src/lib.rs` | 根 preload 库入口与 BPF 初始化。 |
| `src/bpf/main.bpf.c` | `sched_ext` admission scheduler。 |
| `src/bpf/intf.h` | BPF 常量与 task context 布局。 |
| `src/bpf/maps.bpf.h` | task state、thread admission pointer、CPU owner 等 BPF maps。 |
| `src/accordin_shared/src/admission.rs` | 用户态 per-thread admission word。 |
| `src/accordin_shared/src/lock_stats.rs` | CS/NCS/wait 统计与动态并发控制。 |
| `src/accordin_shared/src/cpu_affinity.rs` | CPU mask 解析与动态 affinity 应用。 |
| `src/accordin_shared/src/mutex_hook.rs` | 通用 pthread interposer 与 backend trait。 |
| `src/*_accordin/src/*` | 带 BPF admission 的锁后端 crate。 |
| `src/mcs_tse/src/*` | 使用 timeslice extension 而非 BPF admission 的 MCS 后端。 |
