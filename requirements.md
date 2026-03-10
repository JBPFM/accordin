# 需求文档：基于 sched-ext 的用户态锁竞争感知调度器（scx_ulock）

## 1. 文档目的

本文档定义一个基于 `sched-ext` 的实验性调度器 `scx_ulock` 的需求范围、总体目标、功能边界、实现约束与验收标准。

该项目的目标不是复刻论文中的全部内核实现细节，而是在 **仅跟踪用户态锁** 的前提下，保留论文的核心调度思想：

- 按任务统计锁等待占比；
- 将锁密集任务收敛到 SSC（Special Set of Cores）；
- 在线搜索合适的 SSC 核数；
- 将 SSC 与非 SSC 的调度路径隔离；
- 尽量减少由高并发锁竞争带来的扩展性崩溃。

本项目 **不跟踪内核锁**，也 **不使用 uprobe 在用户态锁热路径上做逐次采样**。

---

## 2. 背景与问题定义

高并发程序中，大量线程在少量热点用户态锁上竞争时，常见问题包括：

- 锁等待时间随线程数上升而快速增长；
- 临界区很短，但等待开销与唤醒/切换开销很高；
- 线程分散在过多 CPU 上运行时，缓存局部性变差；
- 持锁线程与等待线程分布过散，导致共享数据和锁元数据在更多核之间来回迁移；
- 吞吐增长在某一点后不再提升，甚至下降。

参考论文的核心观察是：当锁等待占比持续上升时，将锁密集任务收敛到较小的 CPU 子集，可以提升有效吞吐并减轻扩展性崩溃。论文中锁等待占比按时间片统计，经验阈值为 10%，并使用在线搜索决定 SSC 宽度。fileciteturn8file1L1-L15 fileciteturn8file3L1-L18 fileciteturn8file0L18-L32

本项目将保留这些调度思想，但将测量来源改为 **用户态锁库自采样**。

---

## 3. 总体目标

### 3.1 核心目标

实现一个 `sched-ext` 调度器，使其能够：

1. 基于用户态锁库上报的聚合数据识别锁密集任务；
2. 将锁密集任务调度到 SSC；
3. 将非锁密集任务调度到 SSC 外的普通 CPU 集合；
4. 周期性评估并在线调整 SSC 大小；
5. 在不依赖内核锁追踪的情况下完成闭环控制；
6. 为后续实验提供可控、可重复、可观测的实现基础。

### 3.2 非目标

以下内容不属于 v1 目标：

- 跟踪内核自旋锁、mutex、rwsem 等竞争；
- 修改 Linux 内核源码；
- 面向所有异构工作负载自动达到最优；
- 复杂的能耗模型或节能控制。

---

## 4. 范围

### 4.1 In Scope

- `sched-ext` BPF 调度器；
- 用户态 controller；
- 用户态锁库 `lca_mutex`；
- 基于 mmapable BPF map 的低开销数据同步；
- 基于 epoch 的分类与 SSC 搜索；
- partial mode，仅接管显式选择的任务；
- benchmark 与基础指标采集。

### 4.2 Out of Scope

- 内核锁采样；
- 锁热路径 uprobe/uretprobe 持续采样；
- 全系统默认接管所有任务；
- 多个 SSC 的全局协同；
- 内核补丁；
- 图形化可视化平台。

---

## 5. 总体架构

系统分为三层。

### 5.1 调度层：sched-ext

职责：

- 维护 `ssc_mask` 和 `normal_mask`；
- 维护 `DSQ_SSC` 与 `DSQ_NORMAL`；
- 按任务分类结果做入队与分发；
- 维护最小限度的运行时统计，例如 `run_ns`、迁移次数等；
- 支持基于代际号的懒迁移。

### 5.2 控制层：用户态 controller

职责：

- 加载与控制 BPF 调度器；
- 汇总锁库上报的每线程 epoch 数据；
- 计算 `wait_ratio`；
- 决定任务类别；
- 在线搜索 SSC 宽度；
- 根据拓扑更新 CPU mask；
- 输出调试与指标。

### 5.3 测量层：用户态锁库

职责：

- 提供项目自有锁实现；
- 在慢路径和状态转换点统计锁等待/持锁/阻塞信息；
- 将每线程聚合数据写入 mmap 的共享槽；
- 避免每次加锁都进入内核。

---

## 6. 关键设计原则

### 6.1 仅跟踪用户态锁

系统中与“锁竞争”直接相关的信号，必须由用户态锁库产生。

允许采集的调度辅助信号包括：

- 任务运行时间；
- CPU 利用率；
- 调度迁移次数；
- SSC 宽度变化。

但不允许通过内核锁追踪来补充锁等待指标。

### 6.2 不在热路径依赖 uprobe

`uprobe` 可以用于偶发性调试，但不能作为 steady-state 的锁等待采样主机制。

v1 采样必须依赖：

- 锁库内部本地时间戳；
- per-thread 槽位；
- epoch 聚合；
- mmapable map 共享。

### 6.3 状态机尽量放在用户态

复杂逻辑，如：

- 任务分类；
- SSC 搜索；
- 拓扑策略；
- 行为变化检测；
- 参数调整；

都应放在用户态 controller 中，而不是放在 BPF 热路径中。

### 6.4 优先支持协作式工作负载

v1 默认假设目标程序显式使用项目锁库，属于协作式接入。

---

## 7. 核心功能需求

## 7.1 任务分类

系统必须基于用户态锁等待占比对任务分类。

定义：

- `epoch_runtime_ns`：任务在当前 epoch 的有效运行窗口；
- `user_lock_wait_ns`：该任务在当前 epoch 中等待用户态锁的累计时间；
- `wait_ratio = user_lock_wait_ns / epoch_runtime_ns`。

默认规则：

- `wait_ratio >= 10%` 且 `contended_acq >= 64`，连续 3 个 epoch，则进入 `LOCK_INTENSIVE`；
- `wait_ratio <= 5%`，连续 5 个 epoch，则退出 `LOCK_INTENSIVE`；
- 样本不足时，不迁移，仅保持 `NORMAL` 或 `CANDIDATE`。

该设计保留了论文“按时间窗统计锁等待占比并用 10% 经验阈值识别锁密集任务”的思想。fileciteturn8file1L23-L31

### 7.1.1 任务类别

至少支持：

- `NORMAL`
- `CANDIDATE`
- `LOCK_INTENSIVE`

---

## 7.2 SSC 收敛

系统必须支持将锁密集任务收敛到 SSC。

要求：

- 维护一个连续或尽量紧凑的 `ssc_mask`；
- SSC CPU 只消费锁密集任务队列；
- 非 SSC CPU 只消费普通任务队列；
- 任务在 `ssc_gen` 变化后采用懒迁移，而不是全量同步迁移。

这一点保留了论文“SSC 内外负载均衡分离”的思想。fileciteturn8file0L7-L17

---

## 7.3 在线搜索 SSC 宽度

系统必须在线搜索 SSC 的合理大小。

默认近似目标函数：

- `p = voting_lock_ns / voting_slice_ns`
- `work_cores = min(ssc_width, nr_lock_intensive_tasks)`
- `throughput_proxy = work_cores * (1 - p)`

默认策略：

- 初始 `ssc_width = 1`；
- 若 proxy 连续两次改善，则尝试扩大；
- 若 proxy 连续两次恶化，则回退；
- 成功更新 SSC 后清空或衰减 voting；
- 当锁行为显著变化时，重新进入搜索。

这保留了论文中基于 `T(n)=n*(1-p(n))` 的搜索思路。fileciteturn8file0L29-L40 

---

## 7.4 用户态锁实现

v1 必须实现一把项目自有互斥锁 `lca_mutex`。

### 7.4.1 算法要求

- 快路径：CAS 抢锁；
- 慢路径：MCS 队列；
- 短暂自旋：bounded spin；
- 超预算后：`futex_wait`；
- 解锁：direct handoff + `futex_wake`。

### 7.4.2 必须统计的数据

每线程每个 epoch 至少统计：

- `wait_ns`
- `hold_ns`
- `park_ns`
- `contended_acq`
- `park_count`
- `lock_domain_id`

### 7.4.3 热路径约束

禁止：

- 在自旋循环中反复读时钟；
- 在每次 lock/unlock 时调用 `bpf_map_update_elem()`；
- 记录逐次事件流；
- 在锁热路径上进行重型共享原子更新。

只允许在状态转换点结算一次聚合数据。

---

## 7.5 低开销数据同步

系统必须通过 mmapable BPF map 在用户态锁库与 controller / scheduler 之间同步数据。

### 7.5.1 同步模型

- 每线程固定一个 slot；
- 线程只写自己的 slot；
- controller 周期性读取所有 slot 并汇总；
- controller 生成分类结果并写入任务分类 map；
- sched-ext 仅读取分类结果，不直接参与高频聚合。

### 7.5.2 槽位字段

每个 slot 至少包含：

- `tid`
- `tgid`
- `slot_id`
- `epoch_id`
- `lock_domain_id`
- `wait_ns`
- `hold_ns`
- `park_ns`
- `contended_acq`
- `park_count`
- `seq`
- `flags`

### 7.5.3 一致性要求

需要使用 seqlock 风格版本号：

- 写入前版本号变奇数；
- 写入完成后版本号变偶数；
- reader 只有在两次读取版本号相同且为偶数时才接受快照。

---

## 7.6 sched-ext 调度逻辑

### 7.6.1 必须支持 partial mode

调度器必须支持 partial 模式，仅接管显式设为 `SCHED_EXT` 的任务。

### 7.6.2 最低回调集合

至少实现：

- `select_cpu`
- `enqueue`
- `dispatch`
- `running`
- `stopping`
- `init_task`
- `exit_task`

### 7.6.3 分发规则

- `LOCK_INTENSIVE -> DSQ_SSC`
- `NORMAL/CANDIDATE -> DSQ_NORMAL`
- SSC CPU 只从 `DSQ_SSC` dispatch
- normal CPU 只从 `DSQ_NORMAL` dispatch

### 7.6.4 懒迁移

通过 `ssc_gen` 与 `last_ssc_gen` 实现懒迁移。

禁止在每次 SSC 大小变化时全量强推迁移。

---

## 8. 数据结构需求

## 8.1 全局配置

至少包含：

- `epoch_ns`
- `control_period_ns`
- `enter_threshold_pct`
- `exit_threshold_pct`
- `min_contended_acq`
- `hot_epochs_needed`
- `cool_epochs_needed`
- `ssc_width`
- `ssc_gen`
- `max_ssc_width`
- `partial_mode`

## 8.2 任务上下文

至少包含：

- `pid`
- `tgid`
- `cls`
- `epoch_id`
- `run_ns`
- `runnable_ns`
- `mig_count`
- `last_ssc_gen`
- `hot_epochs`
- `cool_epochs`
- `lock_domain_id`
- `hotness_score`

---

## 9. 拓扑策略需求

SSC 必须是拓扑感知的。

优先级如下：

1. 同 NUMA 节点；
2. 同 LLC 域；
3. CPU ID 尽量连续；
4. 扩容时尽量从边界向外连续扩张；
5. 缩容时优先从边缘回收。

controller 必须从 sysfs 读取 CPU 拓扑信息并形成可复用的排序策略。

---

## 10. 配置需求

controller 必须提供 CLI 参数，至少包括：

```text
--partial
--epoch-ms
--control-ms
--enter-pct
--exit-pct
--min-contended
--hot-epochs
--cool-epochs
--max-ssc
--target-cgroup
--cpu-list
--metrics-out
--enable-rseq-slice-ext
```

所有关键阈值和时序参数都必须可配置，不能只写死在代码中。

---

## 11. 可观测性需求

系统至少需要输出以下指标：

- 总吞吐；
- p50/p95/p99 延迟；
- `wait_ratio`；
- `hold_ratio`；
- `park_ratio`；
- 当前 `ssc_width`；
- `nr_lock_intensive_tasks`；
- 迁移次数；
- SSC CPU 利用率；
- 非 SSC CPU 利用率。

调试信息至少应包括：

- 当前调度器状态；
- 当前 SSC mask；
- 当前分类结果摘要；
- 当前搜索状态；
- 最近一次 SSC 调整原因。

---

## 12. 验收标准

系统满足以下条件时视为通过：

1. 使用项目锁库的工作负载可以在 partial mode 下运行于 `sched-ext`；
2. 用户态锁等待数据能稳定写入并被 controller 正确读取；
3. 不依赖任何内核锁追踪；
4. 分类结果能够驱动 SSC 收敛；
5. SSC 与非 SSC 的分发路径相互隔离；
6. SSC 宽度可以在线调整；
7. sched-ext 退出时系统可安全回退；
8. 在锁密集用户态 workload 上能复现实验趋势：线程数继续增大后 `wait_ratio` 上升，而 SSC 收敛可以改善有效吞吐。

---

## 13. 分阶段交付要求

### Phase 0

- 仓库骨架；
- 最小可运行 `sched-ext` 调度器；
- partial mode；
- 固定 SSC；
- 手工任务分类。

### Phase 1

- mmapable slot；
- `lca_mutex`；
- controller epoch 汇总；
- 基于用户态锁数据的静态分类。

### Phase 2

- SSC 宽度在线搜索；
- 拓扑感知 CPU 分配；
- 懒迁移；
- benchmark 自动化。

### Phase 3

- `lca_rwlock`；
- rseq slice extension（可选）；
- 更好的 lock domain 管理；
- 更完整的文档与测试。

---

## 14. 风险与约束

### 14.1 协作式接入要求

v1 依赖项目锁库，因此目标程序必须显式接入。若应用仍使用原生 `pthread_mutex`，则无法自动获得完整锁等待数据。

### 14.2 短生命周期线程

非常短命的线程可能在积累足够样本前就退出，因此应优先面向线程池或稳定 worker 模型。

### 14.3 工作负载同质性

该方案更适合同质、锁热点清晰的工作负载；异构混合工作负载可能需要额外策略。

论文中的评测工作负载也主要是同质任务。fileciteturn8file2L11-L20

---

## 15. 成功标准

该项目成功的标志不是“所有程序都自动加速”，而是：

- 在可控的用户态锁密集场景下，形成稳定闭环：
  - 锁库采样；
  - controller 分类；
  - sched-ext 收敛；
  - 在线搜索调整；
  - 指标改善可被复现。

如果这一闭环成立，后续才值得继续扩展到更多锁类型、更多工作负载和更复杂的策略。

