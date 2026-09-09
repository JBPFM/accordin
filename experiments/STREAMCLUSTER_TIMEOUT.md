# Streamcluster smoke 超时定位（2026-09-09）

结论：此次 Accordin 384 线程超时是**大量同步操作累计超过 20 秒进程预算**。
延长诊断观察时间后，原始程序可以完成并通过输出校验；没有观察到死锁。
目前没有验证出能在原始参数、384 线程和 20 秒预算下消除超时的 Accordin 修复。
实验参数、正式 runner 的超时规则以及运行前已有的 Accordin 修改均保留。

## 工作量与证据

机器为 96 物理核，所有运行固定 CPU 0–95，384 线程；保持
`10 20 16 512 512 1000 none clusters.txt 384`。

原始二进制第一次补充复现 ROI 为 **31.656 秒**，输出有效。为了区分没有进度与运行缓慢，
该诊断将进程观察上限设为 120 秒；它仍然超过原来的 20 秒预算，不算原 smoke 通过。
[原始复现记录](../results/streamcluster-timeout-repro-20260909/results.jsonl)。

随后用正式 runner、原二进制、关闭诊断计数重复三次（90 秒观察上限），ROI 为
**45.281 / 41.028 / 28.882 秒**，中位数 **41.028 秒**；三次均完成且输出相同，
三次仍全部超出原 20 秒预算。波动明显，所以不把单次候选结果当成稳定的提速或减速。
[重复记录](../results/streamcluster-timeout-repeat-20260909/results.jsonl)、
[重复运行审计](../results/streamcluster-timeout-repeat-20260909/audit.json)。

单独的计数版本只在 PARSEC barrier 的两个阶段结束处加计数，得到：

- **9,758 次 barrier**，每次都有到达、离开两轮广播。
- **7,461,447 次 custody park**，其中 7,459,356 次由 flush 释放，2,091 次到期，
  `parked_now=0`，计数平衡；到期约占 **0.028%**。
- ROI **32.923 秒**；聚类输出与原二进制 SHA-256 一致。

少量点数不等于少量同步：`pFL()` 仍按 `3 * kmax * log(kmax)` 次候选迭代调用
`pgain()`，后者有多处全线程 barrier。两阶段 barrier 在每阶段结束时广播，
近似产生 `2 * (T - 1) * barrier次数` 次等待，随线程数增长。
计数运行的约 746 万次等待与这个规模一致。

源代码还把分块余数全部交给最后一个线程：512 点、384 线程时，383 个线程各处理
1 点，最后一个处理 129 点。这增加了负载不均，但没有单独测量它的贡献，不能把它
当作本次主要耗时的实验证据。

首次 smoke 的 Accordin ROI 为 24/48/96/192 线程对应
1.772/4.071/9.179/17.909 秒。结合 384 线程完成记录，增长大体接近线性。
这些是不同轮次的单样本，只能说明不能把 20 秒超时直接等同于锁性能崩溃，
不能据此证明所有负载下没有退化。

## 性能采样

在单独进程中使用 `perf record -F 99 -g`，未丢失样本。进程 cycles 样本的主要热点：

| 符号 | 样本周期占比 |
|---|---:|
| `_raw_spin_unlock_irqrestore` | 27.95% |
| `scx_dsq_move` | 21.91% |
| `scx_bpf_kick_cpu` | 9.16% |
| `mcs_tas_accordin_direct_mutex_relock` | 4.87% |
| `accordin_wait_notify` | 4.74% |

调用栈把前三项的主要路径定位到
`parsec_barrier_wait → pthread_mutex_unlock → accordin_cv_flush_now → BPF syscall`。
热点是广播释放时的队列搬移、内核同步及 CPU 通知。这里是选定进程的 cycles 采样，
**不是墙钟时间分解**，也不包含所有 idle CPU 上的调度工作。
采样运行 ROI 为 35.040 秒，不并入无采样的时间比较。

[采样数据](../results/streamcluster-diagnosis-20260909/original-perf/perf.data)、
[调用栈报告](../target/streamcluster-diagnose/perf-report.txt)、
[平坦报告](../target/streamcluster-diagnose/perf-flat.txt)、
[阶段计数日志](../results/streamcluster-diagnosis-20260909/original-counted/log.txt)。

## 修复尝试

保持应用二进制、参数、384 线程及 CPU 集合，依次单独运行以下变体。
变体是诊断试验，未混入原来的 smoke 汇总。

| 变体 | ROI 秒 | 结果 |
|---|---:|---|
| 开启 `MOVE\|SPREAD`，flags=24 | 54.620 | 完成，仍超过 20 秒 |
| 关闭 custody，使用既有 futex/重获锁队列 | 84.497 | 完成，仍超过 20 秒 |
| 一批搬移完成后，每个目标 CPU 只 kick 一次 | 38.170 | 完成，仍超过 20 秒；撤回源码修改 |
| 不向执行 flush 的当前 CPU 发 idle kick | 39.552 | 完成，仍超过 20 秒；撤回源码修改 |
| 原生 pthread，作为辅助参照 | 41.389 | 完成，仍超过 20 秒 |

这些都是单次探索，运行波动不小，**没有验证出稳定收益**。不能仅凭它们声称某项修改
一定更慢；可以确定的是它们均未达到原来的 20 秒要求，因此没有保留这些策略修改。
所有完成运行的输出哈希相同。到期占比很小，延长/缩短 custody 到期时间也没有足够依据
作为修复方向。

两份候选构建保留在 `target/streamcluster-diagnose/batched/` 和
`target/streamcluster-diagnose/no-self-kick/`，通过 `LD_LIBRARY_PATH` 选择，
未覆盖正式实验的动态库。[候选哈希与修改说明](../target/streamcluster-diagnose/audit.json)。
`src/bpf/main.bpf.c`、`src/bpf/maps.bpf.h`、`src/runtime.c` 与诊断开始时的副本逐字节相同。

## 复现与后续比较

新增的诊断入口保留原 20 秒预算，另给进程 90 秒观察上限：

```sh
sudo -n python3 experiments/diagnose_streamcluster.py \
  --threads 384 --repeats 3 --budget 20 --timeout 90 \
  --out results/streamcluster-diagnosis-repeat
```

它记录 BPF/动态库加载证据、custody 计数、实际完成时间及输出校验；
`exceeded_original_budget=true` 的运行仍令脚本退出码非零。
可用 `--custody off` 或 `--spread` 复现配置对照，每次使用新的输出目录。
诊断不修改正式 runner 的默认 30 秒或显式 `--timeout 20`。

新入口已实际执行一次：ROI **39.626 秒**，输出校验和 custody 计数平衡检查通过，
动态库/BPF/sched_ext 证据完整，`exceeded_original_budget=true`，脚本按预期退出 **1**。
[入口实跑记录](../results/streamcluster-diagnostic-script-20260909/results.jsonl)。
现有五个 runner 单元检查通过；结束时确认没有遗留 Streamcluster 进程，
sched_ext 为 disabled，正式实验 manifest 中全部产物哈希不变。

继续做性能改进时，应以每轮 barrier 延迟、广播/重新取锁成本和固定输入下的总耗时
作前后对照，并同时验证其他应用；不能用更长超时、减少线程、减少迭代，或只替换
Accordin 的应用 barrier 来宣称修复了锁崩溃。

## 后续 full 工作量

按后续请求执行 FlexGuard 正式参数 `10 30 512 32768 32768 2000`，
Accordin、384 线程、CPU 0–95、一次测量、无额外预热轮、600 秒进程上限。
原实现正常完成，ROI **89.290 秒**、进程墙钟 **89.729 秒**；
输出维度 512、权重总和 32768，通过校验；退出时 sched_ext=disabled，产物哈希不变。
这是另一种工作量的完成结果，不表示原 smoke 的 20 秒超时已经消除。

[full 384 线程记录](../results/streamcluster-full-384-20260909/results.jsonl)、
[full 384 线程审计](../results/streamcluster-full-384-20260909/audit.json)。
