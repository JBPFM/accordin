# Accordin Admission Direct

Accordin 将用户态锁与 Linux eBPF `sched_ext` admission 调度器结合。核心由 C11 锁运行时和 scx C/libbpf 加载器组成，仅保留两个显式 C API 后端：

| 动态库 | 接口 |
| --- | --- |
| `libmcs_accordin_direct.so` | MCS mutex；保留 `mcs_tas_accordin_direct_mutex_*` 兼容别名 |
| `libmcs_tas_accordin_direct.so` | MCS-TAS mutex |

应用通过链接或 `dlopen` 调用 direct API。动态库加载时初始化调度器；它们不导出 `pthread_mutex_*` 或 `pthread_cond_*`，仅设置 `LD_PRELOAD` 无法替换普通程序的 pthread 锁。

普通 pthread 程序可使用 `third_party/litl` 中的标准 [LiTL](https://github.com/multicore-locks/litl) 适配器。该目录保存官方源码及 Accordin 集成，来源版本见 [UPSTREAM.md](third_party/litl/UPSTREAM.md)。`mcsaccordin_original` 和 `mcstasaccordin_original` 分别链接当前的 MCS / MCS-TAS direct 库，共用直接 futex 条件变量，不使用 shadow mutex。

## 构建与验证

```sh
make -j
make check
sudo make check-bpf
sudo make check-claim-bpf
# LiTL pthread 适配器及测试：
make litl
make check-litl
sudo make check-litl-bpf
# 将原始锁函数保留为独立的 perf 符号：
make OUT=target/perf PERF_SYMBOLS=1
# 至少允许两个 CPU 时，额外测试等待期间更改 affinity：
sudo env DIRECT_SMOKE_MIGRATE=1 bash scripts/test_direct_api.sh --bpf
```

构建需要 Clang（包含 BPF target）、bpftool、pkg-config 和 libbpf 开发包（建议 libbpf 1.5+）。例如 Ubuntu 上安装 `clang bpftool libbpf-dev libelf-dev zlib1g-dev pkg-config make`。无需 Rust 或 Cargo。两个库仍输出到 `target/release/`，也可用 `make mcs_accordin_direct` 或 `make mcs_tas_accordin_direct` 单独构建。

scx C 头文件固定在 `third_party/scx/`。构建时从 `/sys/kernel/btf/vmlinux` 生成 `vmlinux.h`，可用 `VMLINUX_BTF=/path/to/vmlinux.btf` 指定其他 BTF。实际调度需要支持 `sched_ext` 的 Linux 内核和 BPF 权限。`check-bpf` 会短时启动调度器，仅在当前没有其他 sched_ext 调度器时运行；可在命令前加 `taskset -c <CPU列表>` 验证受限 affinity。`bash gen-compile-commands.sh` 根据 Makefile 生成 clangd 编译数据库。

应用应在退出所有使用 direct API 的线程后再卸载动态库。MCS 后端保持每线程最多同时持有 4 把锁；调度器支持最多 256 个逻辑 CPU。

### LiTL pthread 接入

```sh
sudo third_party/litl/libmcsaccordin_original.sh ./program
sudo third_party/litl/libmcstasaccordin_original.sh ./program
# 无 BPF：
MCS_ACCORDIN_DIRECT_DISABLE_BPF=1 third_party/litl/libmcsaccordin_original.sh ./program
MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF=1 third_party/litl/libmcstasaccordin_original.sh ./program
```

适配器通过 LiTL 的 `NO_INDIRECTION` 路径在 pthread mutex 中保存内部对象指针，线程节点仍由 direct 运行时管理；构建这两个适配器不需要 CLHT、ssmem 或 PAPI。条件变量支持始终启用，不使用 shadow mutex。waiter 在释放业务锁之前登记，释放后准备一个不占用 CPU 名额的 relock epoch。signal 通知一个已登记 waiter，broadcast 通知全部；被通知者转入 mutex 的停泊队列，首个接力者立即获得 futex wake，其余由接力者的 unlock 逐个唤醒。通知发生在 mutex 外或 mutex 空闲时也能启动接力。

实际唤醒前发布 `USER_WAITING`，现有 BPF enqueue 可直接将其放入 admission 队列——该队列由 `WAITING_SHARDS` 个分片组成，按等待线程所在 CPU 取模选择分片；重获锁复用该 epoch，先检查已有授权，再决定是否 yield。不会把休眠者放入 raw MCS 队列。仍持有其它 mutex 的嵌套等待保留外层 admission，并立即唤醒。普通 lock/trylock 仍直接调用 direct API；unlock 增加一次停泊状态的原子读取，仅存在待接力状态时获取队列 guard。唤醒、超时和 deferred cancellation 均恢复业务锁；取消与 signal 并发时补偿通知其它 waiter，已通知但仍停泊的 waiter 不因原条件变量截止时间到期而丢失通知。这些行为不受 `NDEBUG` 影响。

当前队列设计与测量见 [condvar relock 接力](docs/benchmarks/cond-relock-20260905/README.md)。192 线程的随机读写对比见 [LevelDB readrandom / fillrandom](docs/benchmarks/leveldb-relock-20260905/README.md)。此前实现和 `mutexbench 300,3000` 对照见 [直接 futex 条件变量](docs/benchmarks/litl-futex-cond/README.md)。[shadow 开销测量](docs/benchmarks/litl-shadow-cost/README.md)、[集成迁移记录](docs/benchmarks/litl-upstream/README.md) 和 [此前按需 shadow 优化](docs/benchmarks/litl-lazy-shadow/README.md) 保留为历史记录。

支持进程内普通 mutex 和条件变量（包括静态初始化）、`cond_wait`、`cond_timedwait`、`cond_clockwait`、signal、broadcast 和 deferred cancellation。超时支持 realtime/monotonic，包含现代 C++ `std::condition_variable::wait_for` 路径。显式请求递归、error-check、robust、进程共享或优先级协议 mutex 返回 `ENOTSUP`；mutex timedlock、进程共享条件变量同样返回 `ENOTSUP`。仍有 waiter 时 cond_destroy 返回 `EBUSY`。只支持普通 mutex 的静态初始化器。spinlock/rwlock 保留 libc 实现。MCS 的 4 把嵌套锁上限仍然适用。

`make check-litl` / `sudo make check-litl-bpf` 对两个适配器运行实际符号绑定、并发计数、初始化与复用、嵌套锁、trylock、条件变量 signal/broadcast、两种时钟的超时、错误返回及取消测试，并验证首次等待与并发 lock/trylock、取消时的通知补偿、通知后的取消/超时、持锁外广播、多个 condvar 共用 mutex、C++ 条件变量，以及 `NDEBUG` 下没有原生 shadow mutex 调用。可通过 `LITL_TEST_THREADS`、`LITL_TEST_ITERATIONS`、`LITL_TEST_TIMEOUT` 调整规模。也可在 LiTL 目录运行 `make check` / `sudo make check-bpf`。BPF 测试会短时启动调度器，要求当前 sched_ext 空闲。

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

## Admission 行为

核心规则：**每个逻辑 CPU 只为一个线程保留锁等待名额**。锁本身不再分配 ID，也没有锁数量上限。

1. 线程首次使用 direct mutex 时注册自己的 admission word。无竞争时可以直接获取锁。
2. 外层加锁进入慢路径时，先发布 SPINNING，再查看共享的名额表：本次请求已有授权（condvar 唤醒，或被 flush 送回 bank 后由 dispatch 授权）就直接确认；否则在 `demand`（普通队列与整个 bank 的排队数）不大于 0 且 `ACCORDIN_USER_CLAIM` 开启时，在用户态把自己持有的名额续用到本次请求，或认领当前 CPU 的空闲名额，不进内核；本 CPU 的表项走 restartable sequence，其余情况走 CAS，CAS 认领后还要重读 CPU，若已迁移则撤销。三者都不成立才发布 WAITING 并 yield，进入按 CPU 分片的等待队列。用户态写下的名额由调度器在 enqueue/tick/stopping 收养，此后与内核授权的名额同等对待。
3. CPU 有空闲名额时，从队列中取一个 affinity 允许的线程，先保留名额；用户态确认名额属于本次请求后，才进入原始锁的自旋队列。
4. 每次外层加锁使用递增的请求编号，用户态始终校验本次 ticket。解锁清除用户态状态，调度器观察到释放或未续用的新请求时回收名额，无需额外解锁 syscall。名额保持粘性：下一次慢路径可以在用户态直接续用，也可以在 yield 路径由 BPF 续用到新的请求编号。线程退出时 `exit_task` 按 tid 扫表清除仍留在表中的名额，卸载时 `ops.exit` 统计并清空剩余项。
5. 已获准线程处于 WAITING/SPINNING 且普通队列非空时，tick 结束其当前时间片，为普通任务提供运行机会，同时保留原 admission 名额。这使锁外执行的工作也能继续推进，不改变 raw 锁队列与 condvar 接力协议。

等待队列按 FIFO 扫描；已有名额的线程可以连续续用，因此不保证严格 FIFO 或有界等待。

`ACCORDIN_CV_COUNTERS=1` 时加载会打印 `[accordin_rseq] area=yes/no`，说明这一趟是否真的用 rseq 写名额；卸载会打印 `[accordin_claim] renews= claims= undone= aborts= queued= adopted= swept= slots_left= demand=`：前五项是用户态快路径的续用、认领、迁移撤销、rseq 段被重启和退回排队的次数，后四项是调度器的收养次数、退出扫描清除数、卸载时表中剩余项和 `demand` 残值。`make check-claim-bpf` 用低竞争、过载、关闭开关的过载和持名额退出四个场景检查这些计数。

普通线程与已获准线程都能继续得到调度，因此被抢占的持锁线程可以恢复并解锁。嵌套锁共享一次 admission，最后一把锁释放才结束；持有外层锁的线程不会因获取内层锁再次被 admission 阻塞。若已进入原始锁队列的线程更改 affinity，则允许它完成当前操作，避免将 MCS 前驱停在后继后面。这两类继续执行的路径不属于新等待者的准入限制。

实现只需要一个普通队列、一个新等待者队列，以及每 CPU 一个 owner 记录。已获准线程重新入队时，直接进入名额所属 CPU 的 local 队列。已删除按锁分类、随机选队列、概率参数、扫描预算、序列门控和定时强制放行。

核心不再包含条件变量（CV）、writer-event、width control、锁类合并、动态 CPU affinity、CS/NCS/wait 采样或 timeslice extension。CPU 范围继承应用的 affinity，可通过外部 `taskset` 设置。

## 运行开关

下表中的 `<PREFIX>` 为 `MCS_ACCORDIN_DIRECT` 或 `MCS_TAS_ACCORDIN_DIRECT`。

| 环境变量 | 行为与默认值 |
| --- | --- |
| `<PREFIX>_DISABLE_BPF` | 默认关闭；设为 `1` 时不加载 BPF。 |
| `ACCORDIN_DISABLE_ADMISSION` | 默认关闭；设为 `1` 时不发布 admission 标志。 |
| `ACCORDIN_AUTO_ADMISSION` | 实验性，默认 `0`。设为 `1` 时，首次检测到加载进程的可运行线程数超过可用 CPU 数后启用准入，并保持开启直到卸载。BPF 始终挂载。 |
| `ACCORDIN_USER_CLAIM` | 默认开启；等待队列为空时，慢路径直接在用户态续用或认领本 CPU 的名额，不再 yield。设为 `0` 时始终发布 WAITING 并 yield。 |
| `ACCORDIN_USER_RSEQ` | 默认开启；用户态续用或认领本 CPU 的名额时，比较与写入放在一段 restartable sequence 里，被抢占、迁移或信号打断即重启，不用原子交换。设为 `0`，或 C 库没有为线程注册 rseq 区域时，退回 compare-and-swap。 |
| `<PREFIX>_STATS_ONLY` | 保留历史名称的对照模式；设为 `1` 时加载普通 sched_ext 调度，不进行锁感知路由，也不采样锁时间。 |

自动准入模式在 `runnable/quiescent` 事件中统计加载进程的可运行线程；睡眠线程不计入。初始 CPU 容量取加载线程的 affinity，遇到更窄的线程 affinity 时保守使用较小容量。未触发时，竞争慢路径直接进入原始锁，空闲 CPU 上的唤醒直接投递到 local DSQ。触发后，线程缓存启用状态，新竞争者恢复原准入路径；已经进入 raw 队列的线程保持可运行。无竞争路径保持原来的 epoch/持锁发布，动态判断只放在慢路径。

这是按需启用原型：首次过载后不会自动关闭，不会反复挂载/卸载调度器。未启用准入时也不接管新的条件变量睡眠；跨越启用事件的 relock 仍使用原请求的 epoch。退出时的 `[accordin_auto]` 行记录是否触发、CPU 容量和触发计数。`make check-auto-bpf` 验证两 CPU 下从未过载到过载的持锁、排队和 relock 交接。

目前检测范围是加载进程及其线程，未覆盖其它进程造成的 CPU 竞争、cgroup CPU quota 和完整的异构 affinity 容量计算。因此保持实验性开关，不能将单进程、统一 affinity 的 raytrace 验证推广为通用过载检测保证。

旧 CV、width、固定 width、依赖图、动态 CPU 和采样环境变量已不再解析。CV/writer-event 和动态 CPU 控制的 C 符号及旧时间统计输出已移除。旧 `DEBUG_COUNTERS`、`INACTIVE_PREVIOUS_LOCK_PERCENT` 参数和 `[lock_stats]` 计数输出也已移除。调度异常时，BPF dump 保留队列长度和 CPU owner 信息。

## 实验与历史入口

192 线程下的 Rust → C 迁移性能对比、原始日志和复现命令见 [测量报告](docs/benchmarks/c-migration-192/README.md)。

`fullhook-admission` 与 `simplify` 的 LevelDB readrandom/fillrandom 192 线程对比见 [分支性能报告](docs/benchmarks/leveldb-branches-20260906/README.md)，包含三轮测量及 MCS fillrandom 的超时记录。

逐项归因、选择性迁入和完整应用回归见 [归因报告](docs/benchmarks/leveldb-attribution-20260906/README.md)。本轮仅迁入 tick 普通任务调度机会，192 线程下 readrandom 基本持平，fillrandom 的 MCS/MCS-TAS 分别提升约 54%/188%。

此前的候选方案见 [condvar 授权自旋与 relock 设计](docs/plans/2026-09-05-cv-spin-relock-design.md)。其中 signal 独立唤醒在后续消融中明显回退，未迁入；当前保留 simplify 的通知接力，增加上述 tick 普通任务调度机会。

当前 local 队列与授权更新的合入验证见 [验证记录](docs/benchmarks/local-owner-regression/integration.md)。

实验、benchmark 子模块、第三方源码、结果和历史设计文档保留。mutexbench 的 direct 实验 1、3、6–9 继续使用原库名和 mutex C ABI。实验 4 的 LevelDB direct 选项依赖已移除的 CV/writer-event API，因此已停用；普通 mutex 等外部基线选项和历史结果绘图仍可使用。`patches/leveldb-accordin-direct.patch` 仅保留作历史参考。

依赖已删除的 `accordin`、`mcs_accordin`、`mcs_tas_accordin`、`ttas_accordin`、`reciprocating_accordin` 或 `mcs_tse` preload 的历史入口已停用，会在使用这些库前报错，即使构建目录中仍有旧库。`run_benchmark.sh` 也已停用。新的 pthread 入口使用上述 LiTL 库名；其余外部 benchmark 子模块中的旧说明和脚本保留作历史参考。

实验标签可能仍叫 `mcs_accordin` 或 `mcs_tas_accordin_admission_only`；在支持 direct 的实验中，它们映射到对应 direct 库。历史文档描述的是当时版本，不代表当前功能。
