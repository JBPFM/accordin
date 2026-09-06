# Accordin Admission Direct

Accordin 将用户态锁与 Linux eBPF `sched_ext` admission 调度器结合。核心由 C11 锁运行时和 scx C/libbpf 加载器组成，仅保留两个显式 C API 后端：

| 动态库 | 接口 |
| --- | --- |
| `libmcs_accordin_direct.so` | MCS mutex；保留 `mcs_tas_accordin_direct_mutex_*` 兼容别名 |
| `libmcs_tas_accordin_direct.so` | MCS-TAS mutex |
| `libmcs_accordin_fullhook.so` | MCS 后端的 pthread 拦截库（`LD_PRELOAD`） |
| `libmcs_tas_accordin_fullhook.so` | MCS-TAS 后端的 pthread 拦截库（`LD_PRELOAD`） |

应用通过链接或 `dlopen` 调用 direct API。动态库加载时初始化调度器；direct 库不导出 `pthread_mutex_*` 或 `pthread_cond_*`，仅设置 `LD_PRELOAD` 无法替换普通程序的 pthread 锁。需要在不修改源码的前提下接管普通程序时，改用下文的 fullhook 拦截库。

## 构建与验证

```sh
make -j
make check
sudo make check-bpf
# 将原始锁函数保留为独立的 perf 符号：
make OUT=target/perf PERF_SYMBOLS=1
# 至少允许两个 CPU 时，额外测试等待期间更改 affinity：
sudo env DIRECT_SMOKE_MIGRATE=1 bash scripts/test_direct_api.sh --bpf
```

构建需要 Clang（包含 BPF target）、bpftool、pkg-config 和 libbpf 开发包（建议 libbpf 1.5+）。例如 Ubuntu 上安装 `clang bpftool libbpf-dev libelf-dev zlib1g-dev pkg-config make`。无需 Rust 或 Cargo。四个库都输出到 `target/release/`，也可用 `make mcs_accordin_direct`、`make mcs_tas_accordin_direct`、`make mcs_accordin_fullhook` 或 `make mcs_tas_accordin_fullhook` 单独构建。`make check` 同时校验 direct 与 fullhook 的导出符号并运行两组冒烟测试。

scx C 头文件固定在 `third_party/scx/`。构建时从 `/sys/kernel/btf/vmlinux` 生成 `vmlinux.h`，可用 `VMLINUX_BTF=/path/to/vmlinux.btf` 指定其他 BTF。实际调度需要支持 `sched_ext` 的 Linux 内核和 BPF 权限。`check-bpf` 会短时启动调度器，仅在当前没有其他 sched_ext 调度器时运行；可在命令前加 `taskset -c <CPU列表>` 验证受限 affinity。`bash gen-compile-commands.sh` 根据 Makefile 生成 clangd 编译数据库。

应用应在退出所有使用 direct API 的线程后再卸载动态库。MCS 后端在 direct 库中保持每线程最多同时持有 4 把锁，在 fullhook 库中为 8 把；调度器支持最多 256 个逻辑 CPU。

C 头文件位于 `include/mcs_accordin_direct.h` 和 `include/mcs_tas_accordin_direct.h`。例如：

```c
#include "mcs_tas_accordin_direct.h"

int main(void) {
    mcs_tas_accordin_direct_mutex_t *mutex =
        mcs_tas_accordin_direct_mutex_create();
    mcs_tas_accordin_direct_mutex_lock(mutex);
    /* critical section */
    mcs_tas_accordin_direct_mutex_unlock(mutex);
    return mcs_tas_accordin_direct_mutex_destroy(mutex);
}
```

```sh
cc example.c -Iinclude -Ltarget/release -lmcs_tas_accordin_direct \
  -Wl,-rpath,"$PWD/target/release" -o example
sudo ./example
# 无 BPF 的锁正确性检查：
MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF=1 ./example
```

## pthread 全量拦截（fullhook）

fullhook 库用 `LD_PRELOAD` 接管程序的 pthread 锁，使未修改的二进制（例如 LevelDB `db_bench`）也走同一套 admission 运行时。库被映射时即加载调度器，线程在首次加锁时注册。

被拦截的符号只有 `pthread_mutex_init`、`pthread_mutex_destroy`、`pthread_mutex_lock`、`pthread_mutex_trylock`、`pthread_mutex_unlock`、`pthread_mutex_timedlock`、`pthread_cond_init`、`pthread_cond_destroy`、`pthread_cond_wait`、`pthread_cond_timedwait`、`pthread_cond_signal`、`pthread_cond_broadcast`。

原始锁直接存放在调用方的 `pthread_mutex_t` 内部，不做任何堆分配：`kind` 字段与 glibc 的 `__data.__kind` 对齐在偏移 16，因此 `PTHREAD_MUTEX_INITIALIZER` 的全零对象是未加锁的普通 mutex，`PTHREAD_RECURSIVE_MUTEX_INITIALIZER_NP` 无需 init 调用即为递归 mutex；递归持有只占用一次原始加锁，也只对应一次 admission。条件变量则是 `pthread_cond_t` 内的一个 futex 序号，signal/broadcast 递增序号后唤醒等待者，等待返回后按普通外层加锁重新获取 mutex。

条件变量的等待先尝试自旋：解锁后若本线程没有持有其他被拦截的锁、admission 已启用且调度器已加载，就开一次 admission，带 `USER_CV` 标志发布 WAITING 并 yield 一次；拿到本 CPU 名额时改发布 SPINNING 并直接轮询序号，最长 `ACCORDIN_CV_SPIN_US` 微秒（默认 1000，设为 `0` 关闭自旋，启动时解析一次）。序号在此期间变化就无需任何 syscall；没拿到名额或超出预算则立即按原路 park 到 futex 上。自旋中的等待者不计入 parked 计数，因此唤醒它不需要 syscall。每个条件变量记一个饱和的自旋成绩：序号在预算内变化就减 1，用尽预算、名额被收回或因排队让出则加 2（上限 8）。成绩低于 3 时照常自旋；到达 3 以后，调度器发布的 demand（普通队列与等待队列长度之和）非零就直接 park。自旋中每 64 圈检查一次预算、名额是否仍在，以及（成绩已达 3 时）demand，任一条件满足即停止自旋并 park。这样 barrier 式的条件变量会稳定地 park，而唤醒确实很快的条件变量仍可短暂借用 CPU。

限制：

- 只支持普通与递归 mutex；errorcheck、robust、优先级继承等属性按普通 mutex 处理，`pthread_mutex_timedlock` 直接报错并 `abort()`。
- 条件变量忽略 `pthread_condattr_t`，超时始终按 `CLOCK_REALTIME` 解释，等待不是取消点；`pthread_cond_clockwait` 未被拦截，不要在拦截下使用。
- 只有走 PLT 的调用会被替换；glibc 内部锁以及 `pthread_spin_*`、`pthread_rwlock_*`、`pthread_barrier_*` 不受影响。
- MCS-TAS 的紧凑布局让 `tail` 与 `locked` 共享一个 cache line。
- MCS 后端要求解锁线程即为加锁线程。

运行方式：

```sh
sudo env LD_PRELOAD=$PWD/target/release/libmcs_tas_accordin_fullhook.so ./program
# 无 BPF 的锁正确性检查：
LD_PRELOAD=$PWD/target/release/libmcs_tas_accordin_fullhook.so \
  MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF=1 ./program
# 冒烟测试（C 与 C++ 各一个程序，含拦截生效校验）：
bash scripts/test_fullhook.sh --no-bpf
sudo bash scripts/test_fullhook.sh --bpf
```

`scripts/build_leveldb_stock.sh` 从 `third_party/leveldb-1.23` 的 HEAD 导出未打补丁的源码，构建 `target/leveldb-stock/build/db_bench`（工作区里的 LevelDB 带补丁，不要直接构建）。该二进制配合 fullhook 即可测量未改源码的 LevelDB：

```sh
bash scripts/build_leveldb_stock.sh
sudo env LD_PRELOAD=$PWD/target/release/libmcs_tas_accordin_fullhook.so \
  target/leveldb-stock/build/db_bench --benchmarks=fillseq,readrandom \
  --num=20000 --threads=8 --db=/tmp/accordin-fullhook-smoke
```

环境变量与 direct 库相同：MCS 后端用 `MCS_ACCORDIN_DIRECT_*`，MCS-TAS 后端用 `MCS_TAS_ACCORDIN_DIRECT_*`，`ACCORDIN_DISABLE_ADMISSION` 对两者通用。

### 按锁统计等待来源

`ACCORDIN_HOOK_STATS=1` 打开按 mutex 地址归因的统计，用来区分 LevelDB 里哪些锁走了慢路径、等待时间花在 admission 还是原始锁自旋上。默认关闭，此时加解锁走的是与 direct 库相同的获取函数，只在入口多一次恒定的分支判断，统计代码全部落在冷路径上。

每线程记录最多 64 个地址（第 65 个起并入 `addr=overflow` 行），线程退出与进程退出时合并到全局表，最后按 `admission_ns + spin_ns` 降序输出到 stderr，最多 40 行加上溢出行与合计行：

```
[hook_stats] addr=0x... acq=N fast=N slow=N waits=N yields=N admission_ms=X spin_ms=X hold_ms=X max_hold_us=X cond_waits=N
```

`acq` 是加锁与成功 trylock 的总次数，`fast` 是 `pthread_mutex_lock` 中 trylock 直接成功的次数，`slow` 是进入慢路径的次数，`waits`/`yields` 是进入 admission 等待的次数与其中的 yield 轮数，`cond_waits` 是这把锁被 `pthread_cond_wait` 释放的次数。

## Admission 行为

核心规则：**每个逻辑 CPU 只为一个线程保留锁等待名额**。锁本身不再分配 ID，也没有锁数量上限。

1. 线程首次使用 direct mutex 时注册自己的 admission word。无竞争时可以直接获取锁。
2. 外层加锁进入慢路径时，发布 WAITING 并 yield。若线程仍拥有当前 CPU 的名额，BPF 用 CAS 将旧 ticket 更新为本次请求；否则进入全局 FIFO 等待队列。
3. CPU 有空闲名额时，从队列中取一个 affinity 允许的线程，先保留名额；用户态确认名额属于本次请求后，才进入原始锁的自旋队列。
4. 每次外层加锁使用递增的请求编号，用户态始终校验本次 ticket。解锁清除用户态状态，调度器观察到释放或未续用的新请求时回收名额，无需额外解锁 syscall；若下一次慢路径赶在回收前进入 yield，可以更新并续用自己的名额。
5. 处于 USER_HELD 的线程入队时直接进入其 CPU 的 local 队列并带上抢占标志，避免持锁线程排在普通队列里，而等这把锁的自旋线程正占满 CPU。
6. 已获准线程在自旋等待期间不会让 CPU 变空，`ops.dispatch` 因此不会被调用，普通队列无人消费；tick 时若普通队列非空，就把该线程的时间片清零，让出一次调度给普通队列中的任务，自身保留名额并排到其后。普通队列为空时该路径不做任何事。

调度器带 `SCX_OPS_ENQ_LAST` 标志：否则 CPU 上最后一个可运行线程会被内核直接续跑而不经过 `ops.enqueue`，在其他线程都睡眠时，yield 的等待者永远进不了等待队列，也就拿不到名额；置位后它会正常入队，该 CPU 可能因此空转，这是预期行为。

admission word 的 bit 2 是 `USER_CV`：带该标志的等待者入队时进普通队列而不进等待队列（它靠信号而非名额被唤醒）；yield 时若本 CPU 名额空闲，线程可以直接 CAS 占用，省去入队与 dispatch 的往返。

等待队列按 FIFO 扫描；已有名额的线程可以连续续用，因此不保证严格 FIFO 或有界等待。

普通线程与已获准线程都能继续得到调度，因此被抢占的持锁线程可以恢复并解锁。嵌套锁共享一次 admission，最后一把锁释放才结束；持有外层锁的线程不会因获取内层锁再次被 admission 阻塞。若已进入原始锁队列的线程更改 affinity，则允许它完成当前操作，避免将 MCS 前驱停在后继后面。这两类继续执行的路径不属于新等待者的准入限制。

实现只需要一个普通队列、一个新等待者队列，以及每 CPU 一个 owner 记录。已获准线程重新入队时，直接进入名额所属 CPU 的 local 队列。已删除按锁分类、随机选队列、概率参数、扫描预算、序列门控和定时强制放行。

核心不再包含条件变量（CV）、writer-event、width control、锁类合并、动态 CPU affinity、CS/NCS/wait 采样或 timeslice extension。CPU 范围继承应用的 affinity，可通过外部 `taskset` 设置。

## 运行开关

下表中的 `<PREFIX>` 为 `MCS_ACCORDIN_DIRECT` 或 `MCS_TAS_ACCORDIN_DIRECT`。

| 环境变量 | 行为与默认值 |
| --- | --- |
| `<PREFIX>_DISABLE_BPF` | 默认关闭；设为 `1` 时不加载 BPF。 |
| `ACCORDIN_DISABLE_ADMISSION` | 默认关闭；设为 `1` 时不发布 admission 标志。 |
| `<PREFIX>_STATS_ONLY` | 保留历史名称的对照模式；设为 `1` 时加载普通 sched_ext 调度，不进行锁感知路由，也不采样锁时间。 |
| `ACCORDIN_HOOK_STATS` | 仅 fullhook 库解析；默认关闭，设为 `1` 时按 mutex 地址统计等待来源并在进程退出时输出 `[hook_stats]`。 |

旧 CV、width、固定 width、依赖图、动态 CPU 和采样环境变量已不再解析。CV/writer-event 和动态 CPU 控制的 C 符号及旧时间统计输出已移除。旧 `DEBUG_COUNTERS`、`INACTIVE_PREVIOUS_LOCK_PERCENT` 参数和 `[lock_stats]` 计数输出也已移除。调度异常时，BPF dump 保留队列长度和 CPU owner 信息。

## 实验与历史入口

192 线程下的 Rust → C 迁移性能对比、原始日志和复现命令见 [测量报告](docs/benchmarks/c-migration-192/README.md)。

当前 local 队列与授权更新的合入验证见 [验证记录](docs/benchmarks/local-owner-regression/integration.md)。

实验、benchmark 子模块、第三方源码、结果和历史设计文档保留。mutexbench 的 direct 实验 1、3、6–9 继续使用原库名和 mutex C ABI。实验 4 的 LevelDB direct 选项依赖已移除的 CV/writer-event API，因此已停用；普通 mutex 等外部基线选项和历史结果绘图仍可使用。`patches/leveldb-accordin-direct.patch` 仅保留作历史参考。

依赖已删除的 `accordin`、`mcs_accordin`、`mcs_tas_accordin`、`ttas_accordin`、`reciprocating_accordin` 或 `mcs_tse` preload 的历史入口已停用，会在使用这些库前报错，即使构建目录中仍有旧库。`run_benchmark.sh` 也已停用。外部 benchmark 子模块中的旧说明和脚本保留作历史参考；不属于当前核心支持的运行入口。

实验标签可能仍叫 `mcs_accordin` 或 `mcs_tas_accordin_admission_only`；在支持 direct 的实验中，它们映射到对应 direct 库。历史文档描述的是当时版本，不代表当前功能。
