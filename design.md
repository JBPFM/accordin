# 锁感知协同调度设计文档

## 1. 文档目的

本文档给出一套面向高锁竞争场景的“锁实现 + sched-ext 调度器”协同设计方案。第一版设计强调**实现简单、观测可靠、控制面低开销**，先基于 **per-thread 的锁等待统计** 完成锁感知调度。

目标是：

* 利用 **MCS-TAS** 作为锁侧基础原语，降低 LWP（Lock Waiter Preemption）与纯自旋带来的扩展性崩塌风险。
* 利用 **sched-ext** 在调度层限制过度并发，控制活跃竞争者数量，减少无效自旋、跨 NUMA 交接和错误扩核。
* 保持实现可落地：锁侧仅导出少量 **per-thread 共享状态**，调度器侧使用 per-task 状态与 DSQ 完成 admission control。

该设计适合如下场景：

* 某一组线程持续存在明显的锁竞争。
* 线程可被 sched-ext 接管调度。
* 用户态锁实现可修改，并允许通过共享内存或 map 向 BPF 导出状态。

---

## 2. 设计目标与非目标

### 2.1 目标

1. **降低锁等待导致的 CPU 浪费**

   * 减少大量竞争线程同时活跃自旋。
   * 避免线程数继续增加却只放大锁开销与缓存一致性流量。

2. **降低跨 NUMA 的错误扩核与交接开销**

   * 在 NUMA 感知下优先保留或释放同节点任务。
   * 当等待占比已经很高时，即使存在空闲 CPU，也不盲目扩大发生锁竞争的活跃线程数。

3. **支持基于线程等待占比的并发度控制**

   * 以线程级等待统计为主要反馈信号，对 workload 的活跃线程数进行控制。

4. **控制面低开销、低振荡**

   * 统计以时间窗口和 EWMA 为主，而不是每次 lock/unlock 都做复杂决策。
   * 使用双阈值和最小驻留时间，避免 active set 大小频繁抖动。

---

## 3. 核心设计思想

整体设计分成三层：

### 3.1 锁侧：MCS-TAS

* 无竞争时使用 TAS 快路径，降低常态开销。
* 竞争出现后进入队列化等待，利用 MCS 队列减少广播式竞争。
* 第一版锁侧只维护 **per-thread** 的轻量状态。
* 锁侧额外导出“线程累计等待时间”和“线程当前是否持锁”等最小信息，供调度器观测。

### 3.2 统计层：线程等待占比

* 每个线程维护单调递增的 `wait_ns_total`。
* 调度器在 `running/stopping` 周期内读取其 delta，计算窗口内等待占比。
* `tick()` 可以更高频地触发控制判断，但不重复结算同一段等待时间。
* 第一版统计只依赖线程级等待时间。

### 3.3 调度层：SSC DSQ + NUMA 感知的 admission control

* 调度器维护一个用于暂缓调度的 SSC DSQ，用于承载被抑制的线程。
* CPU 空闲或任务让出 CPU 时，由 `dispatch()` 决定是否从 SSC DSQ 中释放任务。
* 如果当前并发度已经超过控制器计算出的目标值，则即使 CPU 空闲，也不从 SSC DSQ 中继续释放任务。
* 在存在 NUMA 差异时，优先抑制与大多数活跃线程处于不同 NUMA 节点的任务，优先保留同节点任务。

---

## 4. 锁设计

第一版不要求锁侧导出复杂的排队角色。调度器只需要知道线程是否正在持锁，以及该线程累计在慢路径中等待了多长时间。

因此，锁侧最小导出需求为：

* `wait_ns_total`：线程累计锁等待时间。
* `state`：线程当前是否持锁，第一版仅区分 `NONE / OWNER`。

这样做的原因是：

* 接口简单，容易在用户态锁库中落地。
* 能支持“持锁线程尽量不要被抑制”的基本策略。
* 可以先验证等待占比是否足以作为并发度控制信号，再决定是否扩展到更复杂的状态。

---

## 5. 用户态导出接口

## 5.1 每线程共享上下文

不建议只导出一个 `wait_time` 地址。建议每个线程在首次进入锁库时，向调度器注册一块固定布局的共享上下文：

```c
struct lock_sched_thread_ctx {
    u64 wait_ns_total;      // 单调递增累计等待时间
    u32 state;              // NONE / OWNER
    u32 seq;                // 版本号，用于一致性读取
    u32 reserved;
};
```

该结构仅表示**线程级聚合信息**。

## 5.2 注册方式

线程启动后，第一次进入 `pthread_mutex_lock` 封装层时：

1. 初始化本线程 `lock_sched_thread_ctx`。
2. 将该地址注册到 per-task map。
3. 后续调度器通过 task id 找到对应用户态共享结构。

## 5.3 一致性要求

为避免 BPF 读取用户态共享数据时看到中间态，建议：

* 用户态写入前 `seq++` 置奇数。
* 字段写完后 `seq++` 置偶数。
* BPF 端读取时检查前后 `seq` 一致且为偶数。

第一版如果实现复杂，也可先接受近似读取，只要不会影响正确性，仅影响控制精度。

---

## 6. sched-ext 总体结构

## 6.1 设计原则

1. **调度最终在本地 DSQ 生效**。
2. `select_cpu()` 只是优化 hint，不负责最终 admission。
3. 真正的“按策略释放”逻辑放在 `dispatch()`。
4. `running/stopping` 负责时间统计，`tick()` 只负责窗口推进和必要的 reschedule 触发。

## 6.2 调度器工作模式

建议第一版使用 partial switch，只接管目标 workload：

* 目标进程设为 `SCHED_EXT`
* 其他普通线程仍由 fair class 调度

这样可避免外部 CPU 密集任务直接进入你的控制环路。

---

## 7. 核心数据结构

## 7.1 per-task 调度上下文

第一版只保留控制闭环真正需要的字段：能够计算线程等待占比、识别持锁线程、判断线程当前是否已被纳入 active set、以及支持 SSC 滞留时间与 NUMA 策略。

```c
struct task_scx_ctx {
    u64 last_wait_ns;     // 上次读取到的 wait_ns_total，用于计算本窗口 wait delta

    u64 run_start_ns;     // 本次 running 开始时间戳
    u64 run_ns_window;    // 当前窗口累计运行时间
    u64 wait_ns_window;   // 当前窗口累计锁等待时间

    u32 role;             // NONE / OWNER
    u32 admitted;         // 当前是否处于 active set
    s32 last_node;        // 线程最近一次实际运行所在的 NUMA 节点

    u64 ssc_enter_ts;     // 进入 SSC 的时间戳，用于最大滞留时间控制
};
```

各字段必要性如下：

* `last_wait_ns`

  * 必要。
  * 用户态导出的是单调递增的 `wait_ns_total`，调度器必须保存上一次读到的值，才能在 `stopping()` 中计算增量等待时间。没有这个字段，就无法把累计值转成窗口内的 `wait_ns_window`。

* `run_start_ns`

  * 必要。
  * `running()` 和 `stopping()` 之间需要结算本轮实际运行时间。没有这个字段，就无法在 `stopping()` 中计算 `now - run_start_ns`，也就无法得到等待占比的分母。

* `run_ns_window`

  * 必要。
  * 等待占比 `p_t` 需要窗口内运行时间。只记录单次运行时长不够，因为一个窗口内可能经历多次 `running()/stopping()`。

* `wait_ns_window`

  * 必要。
  * 这是等待占比 `p_t` 的分子，也是判断线程是否处于高锁等待状态的直接依据。

* `role`

  * 必要。
  * 第一版虽然只区分 `NONE / OWNER`，但这一位是 admission control 的关键保护条件。没有它，调度器无法保证“持锁线程绝不进入 SSC”。

* `admitted`

  * 必要。
  * 调度器需要快速知道线程当前是在 active set 中还是已经被压入 SSC。理论上可以从队列状态间接推断，但实现复杂且查询代价更高；保留一个显式标志可以让 admission / release 路径更简单、更稳定。

* `last_node`

  * 必要。
  * 文档已经把 NUMA 策略作为第一版目标之一，需要区分 local / remote 的收缩和释放。相比 `last_cpu`，调度控制真正消费的是 NUMA 节点归属，而不是精确 CPU 号，因此保留 `last_node` 即可。

* `ssc_enter_ts`

  * 必要。
  * 文档中已经要求加入“最大滞留时间”和“最小驻留时间”之类的防振荡机制。没有进入 SSC 的时间戳，就无法实现这类时间约束，也难以做“谁先被释放”的时间优先级。

以下字段建议删除：

* `ewma_wait_ratio`

  * 删除。
  * 第一版控制信号是 workload 级的 `p_w`，不是 per-task 的长期平滑值。线程级控制直接基于当前窗口 `run_ns_window / wait_ns_window` 即可，保留 per-task EWMA 会增加维护开销，但不会显著提升第一版控制质量。

* `last_cpu`

  * 删除。
  * 当前设计只需要 NUMA 节点级 locality，不需要 CPU 级 affinity 历史。`last_cpu` 更适合调试、可视化或更激进的局部性优化，不属于第一版必需状态。

* `ssc_reason`

  * 删除。
  * 这是调试字段，不参与 admission、release、统计或正确性约束。第一版应去掉。

* `reserved`

  * 删除。
  * 在设计文档层面没有必要保留占位字段；如果实现中确实需要为对齐补位，再由代码阶段处理。

简化后，这个结构刚好对应第一版的四个核心需求：

1. 通过 `last_wait_ns + run_start_ns + run_ns_window + wait_ns_window` 计算等待占比。
2. 通过 `role` 保护持锁线程。
3. 通过 `admitted + ssc_enter_ts` 实现 SSC 进入、滞留和释放控制。
4. 通过 `last_node` 支持 local / remote 的 NUMA 策略。

## 7.2 per-CPU 调度提示

```c
struct cpu_sched_ctx {
    u32 prefer_local;
    u32 reserved;
};
```

用途：

* 在 NUMA 感知策略下，提示调度器优先选择本地节点任务。
* `dispatch()` 可结合该提示与 active set 状态决定是否从 SSC 释放线程。

---

## 8. 线程角色与状态机

## 8.1 角色定义

### 8.1.1 NONE

线程当前不持有锁。

### 8.1.2 OWNER

线程当前持有锁，处于临界区执行中。

## 8.2 角色迁移

理想迁移顺序：

`NONE -> OWNER -> NONE`

说明：

* 无竞争和有竞争场景都可以抽象为该最小状态迁移。
* 第一版不要求调度器识别等待队列中的细粒度位置。

## 8.3 调度语义

* `OWNER`：绝不放入 SSC。
* `NONE`：按普通调度策略处理。

---

## 9. 统计设计

## 9.1 为什么不只靠 `tick()`

`tick()` 周期较粗，并且只在 CPU 正执行 SCX task 时触发，适合做低频控制，不适合做精确记账。

因此：

* `running()`：记录 run 开始时间。
* `stopping()`：结算本次运行时间，并读取用户态 `wait_ns_total` delta。
* `tick()`：仅用于窗口推进、EWMA 更新、必要时将 `slice = 0` 以触发 reschedule。

## 9.2 窗口统计

每个 task 维护窗口统计：

* `run_ns_window`
* `wait_ns_window`

窗口长度可选：

* 时间窗口：例如 2ms、4ms、8ms
* 或者以若干个 `stopping()` 周期作为窗口

建议第一版使用**时间窗口 + EWMA**，避免过度依赖单个时间片。

## 9.3 等待占比定义

对线程：

`p_t = wait_ns_window / (wait_ns_window + run_ns_window + epsilon)`

对 workload：

`p_w = EWMA(aggregate of p_t)`

即对受控线程集合的等待占比进行聚合，作为 active set 调整信号。

## 9.4 额外统计量

除了等待占比，建议同时维护：

* `slow_rate`：慢路径进入频率
* `runnable_pressure`：当前 runnable 线程数量

第一版至少实现 `wait_ratio + slow_rate` 即可。

---

## 10. SSC / Active Set 设计

## 10.1 使用单一 SSC 队列

第一版采用单一全局 SSC 队列或单一 workload 级 SSC DSQ，用于承载被暂缓调度的线程。

这样做的原因：

* 实现简单，便于先验证线程级等待占比是否足以驱动 admission control。
* 能直接服务于“控制整体活跃竞争者数量”的目标。
* 避免引入额外的对象标识、队列管理和复杂一致性问题。

## 10.2 Admission 对象

允许直接进入 active set 的是：

* 持锁线程
* 一部分普通 runnable 线程

其余线程进入 SSC 暂缓 dispatch。

## 10.3 Active Set 的两层结构

workload 维护：

* `target_local`
* `target_remote`

控制原则：

1. **先收远端，再收本地**
2. **先扩本地，再扩远端**
3. 本地 active set 一般不建议降到 1，以避免临界区交接过慢

---

## 11. NUMA 策略

## 11.1 基本原则

对受控 workload：

* 当前活跃线程主要所在的 NUMA 节点定义为首选节点。
* 优先保留和释放同节点线程。
* 不同节点上的线程优先作为被抑制对象。

## 11.2 收缩策略

当等待占比高时：

1. 优先减少 remote node 上 admitted 线程。
2. 若仍然拥塞，再减少 local node 上普通线程。
3. 不压制持锁线程。

## 11.3 扩张策略

当等待占比降低、慢路径频率回落时：

1. 先增加 local node 的 admitted 数。
2. 仅在本地资源用尽或本地不足以支撑吞吐时，才扩张 remote node。

## 11.4 关于 `(1 - p) * n`

该公式可作为直觉，但不能直接作为最终控制律。建议：

* `p` 使用 EWMA，而非瞬时值。
* `n` 不取全系统总核数，而取受控 workload 可用核数上限。
* 结果再经过 `min_active`, `max_active`, hysteresis 和 NUMA policy 修正。

更稳的形式：

```text
raw_target = clamp(round((1 - p_w) * n_eff), min_active, max_active)
```

其中：

* `n_eff`：受控 workload 可有效使用的 CPU 数
* `min_active`：通常至少为 2
* `max_active`：不超过可用核数与线程数

---

## 12. 控制律

## 12.1 双阈值

对 workload 定义：

* `P_high`：高等待阈值
* `P_low`：低等待阈值，且 `P_low < P_high`

控制规则：

* 若 `p_w > P_high` 且持续 H 个窗口：收缩 active set
* 若 `p_w < P_low` 且持续 L 个窗口：扩张 active set
* 介于两者之间：保持不动

## 12.2 收缩顺序

1. remote 线程优先进入 SSC
2. local 普通线程进入 SSC
3. 绝不压制 owner

## 12.3 扩张顺序

1. 优先从本地节点释放一个线程
2. 本地不足时，再考虑其他 NUMA 节点
3. 每个窗口最多释放有限个线程，避免瞬时放量造成振荡

## 12.4 防振荡机制

必须加入以下保护：

* **最小驻留时间**：线程进入 SSC 后至少停留一段时间才允许再次 admission。
* **最大滞留时间**：线程在 SSC 中不可无限等待。
* **单窗口步长限制**：每次 active set 仅 ±1 或小步变化。

---

## 13. sched-ext 回调分工

## 13.1 `select_cpu()`

职责：

* 提供 CPU 选择 hint。
* 如果发现首选 NUMA 节点存在空闲 CPU，优先 hint 到该节点。
* 不做最终 admission 决策。

原则：

* 不能将其作为强绑定机制。
* 不依赖其保证严格的线程交接局部性。

## 13.2 `enqueue()`

职责：

* 将新 runnable 线程分流到：

  * 普通 DSQ
  * 或 SSC_DSQ
* 若线程是 owner，则直接进入普通路径或本地优先路径。
* 若线程超出 active set，则插入 `SSC_DSQ`。

## 13.3 `running()`

职责：

* 记录 `run_start_ns`。
* 刷新当前 task 的 role / node 信息。
* 若该 task 由 SSC 刚刚释放，可记录 release-to-run latency。

## 13.4 `stopping()`

职责：

* 结算本轮运行时间：`now - run_start_ns`
* 读取用户态 `wait_ns_total`，计算 delta，更新 `wait_ns_window`
* 必要时更新 per-CPU 调度提示

注意：

* 不应假设 `stopping()` 在 task 刚才运行的 CPU 上执行。
* 必须依据 task 的实际运行位置更新其 NUMA 节点归属信息。

## 13.5 `tick()`

职责：

* 推进窗口。
* 更新 EWMA。
* 若检测到本地存在更适合释放的线程，可触发 reschedule。

不建议在 `tick()` 内做大量复杂逻辑。

## 13.6 `dispatch()`

这是整个设计的关键位置。

职责：

1. 查看当前 CPU 所在节点和 active set 状态
2. 若本地节点仍允许扩张，优先从 `SSC_DSQ` 中释放本地线程
3. 若本地为空或本地目标已满，再考虑其他 NUMA 节点
4. 若仍无任务，则走普通 DSQ 或普通 runnable 队列

这样可将“按策略释放”放在真正发生选任务的位置实现。

---

## 14. 调度决策规则

## 14.1 线程进入 SSC 的条件

线程满足以下条件时，可以进入 SSC：

* `role == NONE`
* 当前 admitted 数已超过 target
* 线程不属于当前首选 NUMA 节点的优先保留对象

## 14.2 线程不可进入 SSC 的条件

* `role == OWNER`
* 线程在 SSC 中已接近最大滞留时间
* 线程最近刚从 SSC 释放，处于保护窗口

## 14.3 从 SSC 释放的优先级

1. 同 NUMA
2. 更长等待时间优先
3. 更长 SSC 停留时间优先

---

## 15. 正确性与安全性约束

## 15.1 Watchdog 风险

sched-ext 对长期 runnable 但未被调度的线程存在 watchdog 约束。因此：

* SSC 中线程不能无限滞留。
* 必须设定 `max_ssc_wait_ms`。
* 必须有保底释放路径。

## 15.2 锁语义正确性

调度器只能控制 runnable/admission，不能破坏锁本身的互斥和队列语义。

因此：

* 锁的 owner 识别必须尽量准确。
* admission control 只影响“谁能更快获得 CPU”，不能改变 lock transfer 的内存序语义。

## 15.3 用户态导出失败

若某线程尚未注册共享上下文，调度器应：

* 将其视为 `NONE`
* 走普通路径
* 不能因为缺失 lock 统计而阻塞线程

---

## 16. 第一版实现建议

## 16.1 必选功能

1. MCS-TAS 基础锁
2. `try_lock()` 改为 CAS
3. 导出 `wait_ns_total + role`
4. per-task 窗口记账
5. 单一 SSC DSQ
6. owner 保护
7. 双阈值控制律
8. SSC 最大滞留时间

## 16.2 可后续增加的功能

1. runnable pressure EWMA
2. handoff delay 统计
3. bounded barging
4. 更精细的 remote/local 目标值调节

---

## 17. 参数建议

第一版建议从保守参数开始：

* `window_ns`: 2ms ~ 8ms
* `P_high`: 0.35 ~ 0.50
* `P_low`: 0.15 ~ 0.25
* `min_active_local`: 2
* `min_active_remote`: 0
* `max_step_per_window`: 1
* `max_ssc_wait_ms`: 显著小于 sched-ext timeout

说明：

* 这些值不应硬编码为论文式常数，应允许在线调参与 profiling。

---

## 18. 实现阶段建议

## 18.1 阶段一：验证锁侧统计

目标：确认 `wait_ns_total` 与 role 导出正确。

步骤：

1. 在用户态锁库中加入共享上下文。
2. 打印慢路径等待时间与 owner 切换日志。
3. 验证在无调度控制时统计是否稳定。

## 18.2 阶段二：只做观测，不做 admission

目标：接入 sched-ext，但先不把线程放入 SSC。

步骤：

1. 在 `running/stopping` 中做窗口记账。
2. 在 `tick()` 中输出 per-task 与 workload EWMA。
3. 验证线程级等待占比聚合是否与 workload 行为一致。

## 18.3 阶段三：启用 SSC

目标：实现真正的 active set 控制。

步骤：

1. 启用单一 SSC DSQ。
2. 启用 owner 保护。
3. 只先做 remote 收缩。
4. 再加入 local 收缩和本地优先扩张。

## 18.4 阶段四：NUMA 优化

目标：强化 locality。

步骤：

1. 记录活跃线程分布的主导 NUMA 节点。
2. `dispatch()` 优先释放同 node 任务。
3. 验证跨 node 调度是否下降。

---

## 19. 评估方案

## 19.1 对比对象

建议对比以下基线：

1. 普通 pthread mutex / futex 路径
2. 纯 TAS / TTAS
3. 纯 MCS
4. MCS-TAS 无调度协同
5. MCS-TAS + sched-ext 观测版
6. MCS-TAS + SSC

## 19.2 指标

* throughput
* tail latency
* lock wait time
* slowpath rate
* CPU utilization
* LLC miss / coherence traffic
* remote NUMA 调度次数
* owner->next-owner handoff delay
* runnable stall 次数

## 19.3 工作负载

* 单热锁 microbenchmark
* 混合锁竞争 workload
* 带外部 CPU 密集任务的混合系统
* 不同线程数、不同 NUMA 拓扑
* 不同临界区长度与不同持锁时间分布

---

## 20. 预期收益与风险

## 20.1 预期收益

1. 高竞争时减少无效自旋与过度扩核。
2. 降低 remote node 线程参与度，改善 handoff 局部性。
3. 相比单纯锁优化，更能抑制“调度器把错误线程跑起来”的问题。
4. 相比单纯调度限流，更能结合持锁状态进行控制。

## 20.2 风险

1. 若等待占比不能准确反映真实竞争强度，控制面可能失真。
2. 若 owner 识别错误，会明显影响临界区执行连续性。
3. 若 SSC 滞留时间控制不当，可能触发 watchdog 风险。
4. 锁侧和调度器共享状态的读写若无保护，可能出现观测噪声。

---

## 21. 最终推荐方案

推荐采用以下第一版实现：

1. **锁侧**

   * 使用 MCS-TAS
   * try_lock 改 CAS
   * 导出 `wait_ns_total / role`

2. **统计侧**

   * 以 `running/stopping` 为主记账
   * 按窗口和 EWMA 估计等待占比
   * 以 workload 为聚合单位

3. **调度侧**

   * partial switch 只接管目标 workload
   * 使用单一 `SSC_DSQ`
   * `dispatch()` 时按 `本地 NUMA -> 远 NUMA` 顺序释放

4. **控制律**

   * 双阈值
   * 小步长调节
   * owner 保护
   * 先收远端，后收本地；先扩本地，后扩远端

这套方案实现更简单，也更适合作为第一版原型。

---

## 22. 后续工作

下一步实现时建议先输出以下三个可观察量：

1. 每个线程窗口内 `wait_ratio`
2. 当前 workload 的 `active_local / active_remote / target_local / target_remote`
3. `dispatch()` 中每次释放任务时的 `src_node / dst_cpu / role`

只要这三组观测数据稳定，你的系统基本就具备进入参数调优与论文级实验的条件。


