# Front-Runner Direct Signal Design

## Goal

修复当前 Rust `lb_simple` 的一个关键退化窗口：

- 当前 `front waiter` 被抢占后，后续线程无法直接观察到这件事。
- 只有它的直接后继在运行到本地 MCS 等待循环时，才能发现 `pred == FRONT && pred is preempted`，然后再发布 `queue_bypass`。
- 如果这个直接后继也没有及时运行，后续线程仍会继续排入 MCS 并停留在 `waiting` 自旋里，导致 handoff 时间显著拉长。

目标是让“当前 front waiter 已被抢占”成为一个 lock-local、所有后续线程都能直接观察到的信号，从而尽早退出或绕过 MCS。

## Non-Goals

- 不引入全局 `preempted-front` 计数器。
- 不在本轮实现 epoch drain 或新的 TAS 降流协议。
- 不让 BPF 直接感知具体锁实例或维护 lock-local 状态。
- 不改变当前 `FlexguardQnode` 的 BPF 可见布局。

## Current Problem

当前实现把 `front waiter preempted` 的传播建立在“近邻转发”上：

1. 某个 waiter 成为 `FRONT`。
2. 该线程被 BPF 标记为 `preempted_flags[idx] = 1`。
3. 只有它的直接后继在链接到它、并进入本地等待循环后，才会调用 `request_queue_bypass(pred)`。
4. 在 `queue_bypass` 发布之前，后续线程在 `should_enqueue_mcs()` 中看不到这一状态，仍然会进入 MCS。

这个传播路径的问题是，真正需要知道该信号的往往是“front 后面的整段队列”，但信号却依赖最近的一个 waiter 先运行到特定位置才能扩散。因此，handoff 变长的主因不是状态检查指令本身，而是信号变得对后续线程可见得太晚。

## Recommended Approach

推荐用“当前 front-runner token”替代当前的 `queue_bypass` 机制。

每把锁维护一个新的 lock-local 原子字段：

- `front_runner: AtomicU64`

这个字段存放当前 front waiter 的 `{generation, qnode_index}` token。后续线程不再依赖某个直接后继代为发布 bypass，而是直接读取该字段，结合共享的 `preempted_flags[]` 和 qnode 状态判断：

- 当前是否存在 front waiter
- 该 token 是否仍然匹配当前 qnode 代次
- 该 qnode 当前是否仍是 `FLEXGUARD_CRITICAL_STATE_FRONT`
- 该 qnode 是否已被 BPF 标记为 preempted

只要这些条件同时满足，就认为 `front_runner_blocked()` 为真，线程应停止继续依赖 MCS handoff。

## Alternatives Considered

### 方案 A：保留 `queue_bypass`，只减少检查频率

优点：

- 改动最小。

缺点：

- 不能修复核心传播缺陷。
- 即使把检查做得更便宜，后续线程仍然依赖“直接后继先运行到发布点”。

结论：

- 不采用。它只能减少检查成本，不能解决信号发布过晚。

### 方案 B：恢复更宽的全局 blocking 条件

优点：

- 传播快，所有线程都能立即看到。

缺点：

- 重新扩大退化范围，容易把无关锁或无关队列一起拉出 MCS。
- 回到之前“条件过宽”的问题。

结论：

- 不采用。与前面的协议收缩目标冲突。

### 方案 C：BPF 直接维护 lock-local front-blocked 状态

优点：

- 理论上最直接。

缺点：

- BPF 当前只有线程到 qnode 的映射，没有稳定的锁实例上下文。
- 会显著增加内核态和用户态协议复杂度。

结论：

- 不采用。复杂度和 verifier 风险都不值得。

## Design Details

### 1. Lock State

`McsTasLockRaw` 里的：

- `queue_bypass: AtomicU64`

替换为：

- `front_runner: AtomicU64`

token 编码沿用当前 `generation + index` 的思路，用于防止 qnode 复用导致的悬空命中。

空值语义：

- `0` 表示当前没有已发布的 front runner

### 2. Front Publication Rules

当线程成为当前队列 front 时，发布 `front_runner`：

1. `queue.swap()` 后发现 `pred.is_null()`，说明它是第一个 MCS waiter。
2. 在等待过程中 `qnode.waiting == 0`，说明它刚从前驱 handoff 成为新 front。

发布时写入该线程当前 qnode 的 generation token。

### 3. Front Validity Rules

`front_runner_blocked()` 为真需要同时满足：

1. `front_runner` token 非空。
2. token 可解码为合法 `{index, generation}`。
3. 该 `generation` 仍匹配对应 qnode 当前代次。
4. 该 qnode 当前 `cs_counter == FLEXGUARD_CRITICAL_STATE_FRONT`。
5. 对应 `preempted_flags[index] != 0`。

如果 token 已失效，例如：

- qnode 已复用
- 线程已不再是 `FRONT`
- preempted 标记已清除

则本次调用应尝试把 `front_runner` 清空。

### 4. Lock Admission Rule

`should_enqueue_mcs()` 从当前逻辑：

- `!holder_preempted() && !(queue_bypass active)`

改成：

- `!holder_preempted() && !front_runner_blocked()`

这样新到线程不需要等待近邻 waiter 发布 bypass，就能直接跳过 MCS。

### 5. Queued Waiter Exit Rule

已经进入 MCS 的线程，在本地等待循环中不再检查：

- 自己的 `pred` 是否 blocked
- 是否需要主动发布 `queue_bypass`

而是统一检查：

- `front_runner_blocked()`

一旦为真，就 break 本地 MCS 等待，走现有的 blocking-aware `mcs_exit_blocking()`，然后进入 phase2/TAS 路径。

这样即使它不是被抢占 front 的直接后继，也能及时退出 MCS。

### 6. Unlock / State Cleanup

`unlock()` 仍然清理当前线程的 `HELD` 状态，不需要额外依赖解锁路径发现 waiter 抢占。

`front_runner` 的清理不依赖 unlock；它是懒清理：

- 在 `front_runner_blocked()` 校验 token 时发现失效，则 CAS 清空。

必要时也可以在当前 owner 线程获得锁并 `mark_lock_holder()` 后，尝试清掉仍指向自己的 `FRONT` token，但这不是协议正确性的必要条件。

## Data Flow

1. BPF 在 `sched_switch_btf` 中继续维护每线程的 `preempted_flags[]`。
2. 用户态线程在成为 `FRONT` 时，把自己的 token 写入锁的 `front_runner`。
3. 后续线程在入队前或队列等待期间读取 `front_runner`。
4. 读取后到共享运行时里校验：
   - qnode generation
   - qnode state
   - preempted flag
5. 若判定当前 front 已被抢占，则：
   - 新线程直接绕过 MCS
   - 已入队线程 break 本地 MCS spin，然后通过 `mcs_exit_blocking()` 退出队列

## Error Handling And Concurrency Notes

- 允许 `front_runner` 短暂指向过期 token；通过 generation 校验解决。
- 允许多个线程同时观察到 stale token 并尝试清理；使用 CAS 即可，失败者重读。
- `front_runner` 只是提示信号，不承担 handoff 正确性本身；MCS handoff 仍由 `waiting/next` 和 `mcs_exit[_blocking]()` 负责。
- 任何时候都不能因为 front token 失效而错误唤醒非后继线程；它只能影响“是否继续留在 MCS 队列中”。

## Expected Performance Effect

预计能改善两个现象：

1. 当 `front waiter` 被抢占时，后续线程更早停止依赖 MCS handoff，减少在 MCS 队列中的滞留时间。
2. handoff 指标下降的主要来源应是“信号更早变为可见”，而不是“每轮检查更便宜”。

这不保证 TAS 段的一致性流量一定下降到理想水平，但应当先显著缩短目前由 front-preemption 传播延迟带来的 handoff 尾部。

## Testing Plan

需要补齐这些验证：

1. 当前 front waiter 被抢占时，新来的线程会在 `should_enqueue_mcs()` 处直接跳过 MCS。
2. 非直接后继但已经排队的 waiter，在 `front_runner_blocked()` 生效后也会 break 本地 MCS 等待。
3. qnode 复用后，旧 token 不会误触发新的 front-blocked 判定。
4. 一把锁上的 front-blocked 状态不会污染另一把锁。
5. 现有 `mcs_exit_blocking()` 的 late-successor handoff 协议继续成立。

## Implementation Outline

1. 用 `front_runner` 替换 `queue_bypass` 字段和相关 helper。
2. 增加 front token 的发布、判定、懒清理逻辑。
3. 把 `should_enqueue_mcs()` 和 phase2 条件改成读取 `front_runner_blocked()`。
4. 删除等待循环里基于 `pred` 的 bypass 发布逻辑。
5. 补单元测试和 source-contract test。
6. 重新跑当前 64-thread `mutex_bench` 场景确认 handoff 是否回落。

## Open Questions

- 是否还需要在获得锁后主动清理指向自己的 `front_runner` token，以减少 stale-token 重读次数。
- 当 `front_runner_blocked()` 长时间为真时，是否还要配合第二阶段 epoch/drain 机制减少 TAS 一致性流量。

这两个问题都不影响本轮修复的正确性，可以在本轮验证后再决定是否继续优化。
