# Streamcluster：192 线程性能差距诊断

**主要原因是当前 admission 策略在密集 barrier 上的开销。** 完整负载中只设置 `ACCORDIN_DISABLE_ADMISSION=1`，保留 BPF、原锁和原条件变量，MCS 从原三轮均值 400.28 s 降为单次诊断的 81.11 s，MCS-TAS 从 364.11 s 降为 79.12 s。FlexGuard 原三轮均值为 178.84 s。

| 192 线程，同一完整负载 | MCS-Accordin | MCS-TAS-Accordin |
|---|---:|---:|
| 原配置，admission 开启，三轮均值 | 400.277 s | 364.112 s |
| 仅关闭 admission，单次消融 | 81.114 s | 79.120 s |
| 原均值 / 消融用时 | 4.93× | 4.60× |

两次消融均确认实际 Accordin 库映射、BPF fd、sched_ext 启用、enable_seq 增加一次，以及退出后 scheduler 卸载。输出均为 46,272 B，SHA-256 与此前 192 线程、三种锁的九次正式运行完全相同：[输出核对](output-verification.json)。该 SHA 一致性支持工作量及结果相同，不代替独立的聚类算法正确性证明。

这是单次诊断消融与既有三轮基线的对比，不把它当成新一轮三次重复的正式排名。17 个既有程序/库哈希均未改变；没有把诊断 shim 写入 LiTL 或改变默认配置。

## 放大路径

1. 本构建启用 `ENABLE_AUTOMATIC_DROPIN`，关闭 `ENABLE_SPIN_BARRIER`，实际使用 PARSEC 的集中式、两阶段 `parsec_barrier_wait`。所有线程争用同一把 mutex；最后到达者广播，最后离开者再次广播。
2. `pgain` 每次调用包含 **9 次 barrier**，`pFL` 又反复调用 `pgain`。这不是单纯持续计算或持续持锁的负载，而是计算、集体睡眠、集体唤醒和重新抢锁反复交替。
3. 在 `src/direct.c` 中，外层竞争加锁先执行 `admission_wait`，取得 CPU 的 ticket 授权后才进入 raw lock。`src/runtime.h:41` 中的等待循环反复 `sched_yield()`，读取当前 CPU 的 `owners[cpu]`，直到 ticket 相符。
4. 每次 cond wait 都会释放外层 mutex、结束当前 episode，醒来重抢锁时可能开始新的 admission 请求。小临界区不断重复授权、让出 CPU、排队和锁交接；在 96 个物理核上同时运行 192 个 worker 时，这些成本被放大。
5. BPF 的等待队列和普通队列为全局 DSQ；获取授权还要维护 per-CPU owner。高频请求及线程迁移是额外的成本来源。现有采样和消融确认 admission 整条路径代价大，但没有把其内部分解成精确的“DSQ、ticket、迁移各占多少秒”。

FlexGuard 的慢路径通过其 BPF 监测的持锁者抢占状态在自旋与 futex 阻塞之间切换，没有 Accordin 这条逐次 `sched_yield` 授权循环。本次不是 TSE/FLEXGUARD_ALL 配置。

## CPU 采样和系统调用

使用未修改的完整 Streamcluster 参数启动，在启动 3 秒后采样 30 秒；每个后端串行运行。采样结束主动终止诊断进程，因此这些是**运行前段的 CPU 样本及计数，不是完整 ROI 时间分解**。`perf record -e cpu-clock -F 99` 与 `perf stat` 同时运行，tracepoint 本身也会带来测量开销。

| 30 秒采样窗口 | MCS | MCS-TAS | FlexGuard |
|---|---:|---:|---:|
| sched_yield 调用 | 5,479,804 | 9,540,005 | 0 |
| CPU migrations | 847,168 | 854,004 | 354,798 |
| context switches | 1,187,249 | 1,138,463 | 2,109,848 |
| 加锁函数 CPU 样本比例 | 44.15% | 48.41% | 94.81% |
| `__schedule` + `finish_task_switch` 样本比例 | 28.52% | 27.88% | 两者均低于报告的 0.3% 显示阈值 |

不能把原因简单写成“Accordin 上下文切换更多”：这个窗口中 FlexGuard 的切换次数反而更多。区别是 Accordin 有大量主动 yield、更多迁移，且主要调度函数占据较高 CPU 样本比例。也不能用“谁的锁函数百分比更高”直接推导谁的整体性能更差；样本比例不等于有效进度或锁等待的墙钟时间。

对 Accordin 加锁函数的汇编标注显示，函数内部的绝大多数样本落在等待前驱的 `isb / ldarb / tbnz` 自旋循环。即加锁函数的 44%/48% 主要是 raw MCS 队列等待，不应全部记成 admission 函数自身的 CPU 时间；关闭 admission 大幅缩短完整运行，说明它也影响后续排队与交接的整体成本。

MCS 库保留历史 `mcs_tas_accordin_direct_mutex_lock` 符号别名，perf 可能显示该名字。实际 DSO 映射已确认是 `libmcs_accordin_direct.so`，没有混用两种锁。

## 条件变量差异及消融

当前 Accordin 每个 cond waiter 有独立 futex。`accordin_cond_broadcast` 持有 guard，循环对每个 waiter 执行 `FUTEX_WAKE_PRIVATE(...,1)`。满员 192 线程 barrier 的到达阶段有 191 个等待者，需要 191 次 wake 系统调用。barrier 此时还持有用户 mutex，广播结束前醒来的线程不能完成重获锁。

FlexGuard 使用共享序号并调用一次 `FUTEX_WAKE_PRIVATE(...,INT_MAX)`。这是系统调用次数的差异；内核唤醒 N 个线程本身仍有随 N 增长的工作，不能说其总成本是 O(1)。

为了判断这个差异是否主导，使用同一份 PARSEC barrier 对象文件，192 线程连续执行 100 轮，只做独立微基准。临时 `bulk_cond.so` 对齐 FlexGuard 的共享序号 wait/broadcast 思路，仅适用于此 barrier 子集；它也省略取消处理，不是通用 POSIX condvar，更不是生产修复。

| 100 轮 barrier，单次诊断，包含线程创建/回收 | MCS | MCS-TAS |
|---|---:|---:|
| 原 cond + admission | 1.833 s | 1.376 s |
| 简化批量 cond + admission | 2.139 s | 2.248 s |
| 原 cond，关闭 admission，BPF 保留 | 0.369 s | 0.377 s |
| 简化批量 cond，关闭 admission，BPF 保留 | 0.328 s | 0.334 s |

对应的 FlexGuard 为 0.781 s。微基准持续时间短、只有一次，不能解释小差异或统计显著性；它用于筛选主因，随后用完整 Streamcluster 消融验证。

批量 cond 确实将整个微基准的 futex wake 系统调用从约 38,000 次降到约 200 次，但 **admission 开启时没有改善耗时**。只关闭 admission、保留原 cond 就明显加快。因此，不能把“逐 waiter 唤醒”当成这次 2× 差距的主要解释；它是可以继续优化的成本项。

## 192 线程相对 96 线程

原正式数据中，96→192 线程的耗时增长分别为 MCS 3.38×、MCS-TAS 4.20×、FlexGuard 9.39×。三种实现都受到超额线程及密集同步影响；192 线程下 Accordin 相对 FlexGuard 的差距实际上小于 96 线程。机器只有 96 个物理核、无 SMT，增加到 192 个 worker 没有增加计算资源。

## 后续优化方向

优先评估 admission 对短临界区、condvar 重获锁和突发 barrier 竞争是否应延后或绕过，而非每次外层竞争都立刻进入 yield/授权循环；进一步减少无效 yield 和 CPU 迁移。共享 futex / 分组批量广播可作为后续 condvar 优化，但须保留取消、超时和 waiter 生命周期语义。这些方向尚未实现，也不据此推导其他负载都应关闭 admission。

## 数据与复现

- [完整负载消融记录](native-ablation.jsonl)、[系统调用与 barrier 用时 CSV](counters.csv)、[配置及哈希](metadata.json)。
- `prefix-192-*.report.txt` 为 CPU 采样报告，`*-lock-annotate.txt` 为汇编标注；[perf 原始数据](profiles.tar.gz)。所有日志及脚本都在本目录。
- 诊断工作目录：`target/streamcluster-analysis-20260905/`。既有构建目录：`target/flexguard-suite-20260905/`。
- `diagnose.py` 运行原 barrier 和 30 秒采样，`ablation.py` 运行 barrier 消融，`native_ablation.py` 运行完整负载的 admission 消融；均使用原 suite 的环境清理规则和 flock 串行化。脚本中的限时主动终止仅用于诊断。
- 完整负载始终为 `10 30 512 32768 32768 2000 none output 192`；计数器按实测 100 MHz 换算。

构建诊断组件（假设原 suite 构建目录存在；将本目录源文件复制到诊断工作目录）：

```sh
g++ -O3 -DNDEBUG -pthread -I target/flexguard-suite-20260905/streamcluster target/streamcluster-analysis-20260905/barrier.cpp target/flexguard-suite-20260905/streamcluster/parsec_barrier.o -o target/streamcluster-analysis-20260905/barrier
gcc -O3 -fPIC -shared -pthread target/streamcluster-analysis-20260905/bulk_cond.c -o target/streamcluster-analysis-20260905/bulk_cond.so
sudo python3 target/streamcluster-analysis-20260905/native_ablation.py
```
