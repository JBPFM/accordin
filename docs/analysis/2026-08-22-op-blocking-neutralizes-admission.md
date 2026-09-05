# Op 内阻塞使 admission 收益归零：chain 模式 nanosleep 判别结果

日期：2026-08-22
数据：`experiments/results/diag_chain_multilock_20260822/`（61 次运行）
基准：`bench/mutexbench/multilockbench --chain`（本轮新增，复刻 readrandom 的 op 结构）
关联：`docs/analysis/2026-08-20-cv-diagnosis-m0.md`（readrandom 阻塞测量）

## 结论

1. 当 op 体内包含**调度器可见的阻塞**（nanosleep 工作段）时，BPF admission 对 hook 路径的全部吞吐收益归零：spin 工作段下 admission 相对裸 pthread 赢 1.65–1.94x，nanosleep 下收益为 0.96–1.03x（打平）。
2. 阻塞不会引发塌陷——它只是让 admission 无事可做。塌陷（138–400x）只出现在无 BPF 的纯自旋 hook 里。
3. 唯一一次 BPF 开启下的间歇性劣化（0.15x）恰好发生在 nanosleep arm，且没有任何 BPF 计数器异常——与 readrandom 双峰慢模式的"无签名"特征一致，但只出现一次，未复现。
4. **nanosleep 机制不能解释 readrandom 的 BPF-on 塌陷**：readrandom 的 op 体内实测几乎没有阻塞（见下），它与 chain+spin 同属"无阻塞"类，但两者在 BPF 开启下表现相反。readrandom 塌陷的触发因素仍未被合成复现。

## 实验设置

chain 模式每 op 顺序执行 6 次加锁（hot 锁 → 池 A 分片 → 池 B 分片 Lookup → 工作段 → 同一 B 分片 Release → 同一 A 分片 Release → hot 锁），1+17+17=35 把锁，每步临界区 150ns，锁间无嵌套，工作段（默认 30µs）在无锁状态下执行。工作段三种形态：

- `spin`：纯 CPU burn，线程永不离开 runqueue；
- `nanosleep`：`clock_nanosleep` 主动睡眠 30µs，线程真实阻塞；
- `pread`：对页缓存热的文件做 4KiB 随机 pread（系统调用 + memcpy，不睡眠）。

arm：P（裸 pthread）、TB（`libmcs_tas_accordin.so` + BPF）、MB（`libmcs_accordin.so` + BPF）、TN（mcs_tas hook、BPF 关，塌陷对照）。

## 数据

256 线程（96 CPU，2.67x 超订），中位数：

| 工作段 | P (ops/s) | TB | MB | TB/P | MB/P |
|---|---:|---:|---:|---:|---:|
| spin | 548,272 | 902,790 | 931,997 | **1.65x** | **1.70x** |
| nanosleep | 519,517 | 526,249 | 499,517 | **1.01x** | **0.96x** |
| pread | 486,984 | 869,405 | 945,035 | 1.79x | 1.94x |

要点：

- pread 与 spin 同类（1.65–1.94x），与 nanosleep 不同类——区分变量是**是否睡眠**，不是**是否进内核**。
- TN（BPF 关）在 spin 下塌 138–400x，拐点精确落在线程数=CPU 数（96t 时 TN 是全场最快 1.49x，128t 即塌 400x）；塌陷态是稳态且随时间加深（30s 时 481 ops/s，p50 692ms）。
- nanosleep 的间歇异常：MB r2 = 78,559 ops/s（0.151x P），r1/r3 正常（0.96x/0.97x）。该次运行计数器只有等比缩小、无任何 BPF 侧异常签名。

## 计数器签名

nanosleep 是唯一让唤醒路径成为主导的工作段：

| 计数器（TB，256t） | spin | nanosleep |
|---|---:|---:|
| running_pending_grant success/failure | 371,766 / 494,871 | 2,991,934 / 2,969,455（约 8 倍流量） |
| wake_read_fail | 0 | **137,987（唯一非零的工作段）** |

另一个方法论要点：**grant failure 比率不是塌陷标志物**。TB 的 30s spin 运行 failure:success 达 1:3 而吞吐与 10s 完全一致；此前把 readrandom 慢模式的 10:1 当判据是过度解读，它是伴随症状。

## 机制解读

admission 的收益来自管住"自旋等待者占满 CPU"：把超额的自旋者从 runqueue 挪走，让持锁者和队首不被抢占。前提是**线程在等锁时留在 CPU 上自旋**。当 op 体内有真实睡眠时：

- 线程每 op 主动让出 CPU 一次，等价于自带"admission 释放点"——`accordin_stopping`（`!runnable`）本来就会回收 token；
- 唤醒路径（`wake_*`）取代 enqueue-grant 成为主要调度事件，per-CPU owner 槽位高速翻转（grant 流量 8 倍）；
- 自旋压力本身也被睡眠稀释（256 线程 × 30µs 睡眠 ≈ 任一时刻只有部分线程在竞争 CPU）。

于是 admission 既没有可管的自旋拥塞（收益消失），也没有被它破坏的东西（不塌陷）——净效果为零。

## 与 readrandom 的关系：排除而非解释

readrandom 的 op 体内阻塞已有直接测量（M0 诊断，`scripts/futex_block_by_uaddr.bt`）：

- 全程仅 **354 次 FUTEX_WAIT**（对比 fillrandom 的 158 万次），98.1% 的阻塞时间在 harness 的 start/stop barrier 上，不在 op 体内；
- 塌陷超时运行的快照：256/256 工作线程全部 R 态——塌陷时在自旋，不在睡眠；
- pread 页缓存全热（DB 几十 MB），本轮矩阵已证 pread 类工作段行为等同 spin。

因此 readrandom 属于"无阻塞"类。而 chain+spin（同为无阻塞、同锁结构、同超订）在 BPF 开启下 40/40 健康。**readrandom 的 BPF-on 间歇塌陷必然来自 chain 尚未模拟的其他结构**，按嫌疑排序：

1. **工作段异构性**：readrandom 的 op 时长双峰（block cache 命中 ~1µs vs 未命中 pread+解压几十 µs），chain 用固定 30µs。异构到达节奏产生突发批量竞争，admission 的 per-CPU 槽位对突发更易饱和。
2. **hold 时长异构**：DB mutex 与 cache 分片的持锁形态不同（chain 统一 150ns）。
3. 真实嵌套（持 DB mutex 时进 env mutex）——频率低，嫌疑最小。

下一个最便宜的判别器：给 chain 工作段加双峰分布（如 90% × 1µs / 10% × 40µs，保持均值不变），其余参数不动。

## 对设计的含义

- admission 的保护模型隐含假设"等待者以自旋形式在场"。op 内会阻塞的工作负载（写路径、cv 密集、真实 IO）天然落在该模型之外——这与既有结论一致：fillrandom 的收益来自 CV_SLEEP 路由与 WriterEvent（M1/M3），而非 spin admission 本身。两套机制覆盖两类负载，边界就是"op 内是否睡眠"。
- 混合负载（同一进程内既有自旋竞争又有阻塞 op）下 owner 槽位被唤醒路径高速翻转的行为值得单独评估，本轮 8 倍 grant 流量未造成损害，但只测了纯 nanosleep 一种形态。

## 数据可信度备注

- 塌陷判定基于吞吐与 p50；本轮期间 bench 已修复塌陷态百分位样本不足的问题（改为每 op 采样，样本 <1000 时拒绝输出 p99/p999），修复前的塌陷行 p99/p999 不可用，p50 与吞吐不受影响。
- chain 模式的 `avg_wait_ns_estimated`（现已更名 `avg_non_hold_ns_per_op`）包含整个无锁工作段，只可用于同工作段的 arm 间对比。
- 机器不空闲（其他用户负载在跑），但 arm 内重复离散度 ≤3%，且裸 pthread 对 5 倍 load 波动不敏感（三次运行差 0.06%），组间差异远大于噪声。
