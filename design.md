# 锁感知调度器设计文档

## 1. 文档目的

本文档描述 `accordin` 当前已经实现的锁感知调度器设计，而不是早期的 `target_local / target_remote` 目标并发度方案。

当前实现的核心目标是：

- 在高锁竞争负载下，用锁等待信号识别“应该降并发”的线程。
- 把明显在等锁的线程暂时放入 `SSC_DSQ`，把 CPU 让给更可能做有用工作的线程。
- 用一组可动态伸缩的 “SSC core” 专门从 `SSC_DSQ` 拉任务运行，形成简单、可测的反馈回路。
- 保持用户态锁路径尽量轻量，使 `ACCORDIN_DISABLE_BPF=1` 和 `ACCORDIN_STATS_ONLY=1` 成为可靠的拆分基线。

当前实现不追求：

- 精确的 per-lock owner / waiter 拓扑。
- 完整的 NUMA-aware local/remote target 控制。
- partial switch；当前仍是 sched-ext 全局接管。

---

## 2. 总体结构

`accordin` 由三个部分组成：

1. 用户态锁库：
   - 以 `LD_PRELOAD` 形式拦截 `pthread_mutex_*` / `pthread_cond_*`。
   - 用 MCS-TAS 代替原始 `pthread_mutex_t` 的互斥实现。
   - 为每个线程导出锁等待上下文，供 BPF 周期性读取。

2. sched-ext 调度器：
   - 维护 `READY_DSQ` 和 `SSC_DSQ` 两个队列。
   - 普通线程优先走 `READY_DSQ`。
   - 被判定为“锁等待占比过高”的线程被标记为 `admitted=0`，随后重新入队时进入 `SSC_DSQ`。

3. 反馈控制回路：
   - 非 `SSC core` 上运行的线程如果在当前窗口内 `wait_ns_window > run_ns_window / 10`，会在 `tick()` 中自 parking。
   - `SSC core` 负责从 `SSC_DSQ` 拉任务，并汇报“当前活跃 SSC core 数是否带来更高有效工作量”。
   - 调度器以 200ms 窗口收集投票，按 2 倍/2 分之一调整 `ssc_active_count`。

设计上的关键点是：当前实现不再显式维护 “活跃线程目标值”，而是通过 “多少个 CPU 负责消费 `SSC_DSQ`” 间接限制锁竞争并发度。

---

## 3. 用户态锁设计

## 3.1 MCS-TAS 互斥体

当前锁实现位于 [`src/mcs_tas.rs`](./src/mcs_tas.rs)。

- 快路径：先做 TAS；无竞争时直接拿锁。
- 慢路径：进入 MCS 队列，等待前驱唤醒，再抢占 TAS 位。
- `try_lock()` 使用 CAS，避免无谓的 cache-line 失效。
- 竞争锁成功后可申请 timeslice extension；解锁时归还扩展时间片。

这层的目标不是做复杂调度决策，而是稳定暴露“线程什么时候开始等锁、什么时候停止等锁”。

## 3.2 每线程导出上下文

当前用户态导出的结构如下：

```c
struct lock_sched_thread_ctx {
    u64 wait_ns_total;
    u64 wait_start_ns;
    u64 wait_end_ns;
};
```

语义如下：

- `wait_ns_total`：
  - 已完成的慢路径等待时间累计值。
  - 单调递增。
- `wait_start_ns`：
  - 本次慢路径等待开始时间。
- `wait_end_ns`：
  - 最近一次已完成等待的结束时间。
  - 当 `wait_end_ns < wait_start_ns` 时，BPF 侧把该线程视为“当前仍在等待”。

与早期设计不同，当前实现没有导出 `role` / `OWNER` 状态，也没有 seqcount。

## 3.3 线程注册

线程第一次进入拦截后的 `pthread_mutex_lock()` 时：

1. 初始化线程本地 `lock_sched_thread_ctx`。
2. 调用 `prepare_thread_timeslice()`。
3. 把 `tid -> ctx_ptr` 注册到 `thread_ctx_addr_map`。
4. 在线程退出时删除该 map 项。

因此，只有真正使用 accordin 锁路径的线程才会被 BPF 建立 `task_scx_ctx`。

---

## 4. BPF 调度器结构

## 4.1 队列模型

调度器维护两个 DSQ：

- `READY_DSQ`
  - 正常 runnable 线程。
- `SSC_DSQ`
  - 已被暂时压制、等待再次放行的线程。

当前实现的语义是：

- 新线程默认 `admitted=1`。
- 一旦线程在某个窗口里显示出明显锁等待，它会在 `tick()` 中把自己标记为 `admitted=0` 并主动缩短 slice。
- 该线程后续重新入队时会进入 `SSC_DSQ`。
- 只有被选中的 `SSC core` 会优先从 `SSC_DSQ` 取任务。

## 4.2 per-task 状态

当前 per-task 状态如下：

```c
struct task_scx_ctx {
    u64 window_epoch;
    u64 last_wait_ns;
    u64 pending_wait_ns;
    u64 run_start_ns;
    u64 run_ns_window;
    u64 wait_ns_window;
    u32 admitted;
    u64 user_ctx_ptr;
};
```

各字段职责：

- `window_epoch`：
  - 当前 task 记账属于哪个投票窗口。
- `last_wait_ns`：
  - 上次读到的 `wait_ns_total`。
- `pending_wait_ns`：
  - 当前仍在进行中的等待时间，避免 `tick()` 重复累加。
- `run_start_ns`：
  - 最近一次开始运行的时间戳。
- `run_ns_window` / `wait_ns_window`：
  - 当前窗口内的运行时间与锁等待时间。
- `admitted`：
  - 1 表示重新入队时走 `READY_DSQ`，0 表示走 `SSC_DSQ`。
- `user_ctx_ptr`：
  - 缓存的用户态 `lock_sched_thread_ctx` 地址。

这里同样没有 `OWNER`、`last_node`、`target_local` 之类字段。

## 4.3 Maps 与全局变量

当前控制回路依赖以下 map / 全局变量：

- `task_ctx_map`
  - `BPF_MAP_TYPE_TASK_STORAGE`，存放 `task_scx_ctx`。
- `thread_ctx_addr_map`
  - `tid -> user_ctx_ptr`。
- `stats_map`
  - 导出窗口统计和调试计数。
- `cpu_to_node`
  - CPU 到 NUMA node 的映射。
- `agg_percpu_map`
  - 每 CPU 的 `run_ns` / `wait_ns` 累加器。
- `ssc_vote_slot_map`
  - 每个 active `SSC core` 在当前窗口中的投票快照。

关键全局变量：

- `ssc_vote_window_ns = 200ms`
- `ssc_active_count = 2`
- `ssc_cpu_count`
- `ssc_cpu_list[]`
- `ssc_cpu_rank[]`
- `stats_only_mode`
- `ssc_vote_*` 一组窗口内汇总量和迟滞计数器

---

## 5. 锁感知控制律

## 5.1 等待时间记账

当前实现只在 `running()` 和 `tick()` 之间记账，没有启用 `stopping()` 回调。

`account_task_activity()` 的逻辑是：

1. 用 `now - run_start_ns` 计算 `run_delta`。
2. 读取用户态 `lock_sched_thread_ctx`。
3. 用 `wait_ns_total - last_wait_ns` 结算已经完成的等待。
4. 如果 `wait_end_ns < wait_start_ns`，说明线程此刻还在等锁，再把 `now - wait_start_ns` 的增量累加到 `pending_wait_ns`。
5. 更新当前 task 的窗口统计，并顺手累加到 per-CPU 聚合器。

因此，当前等待信号覆盖两类情况：

- 已结束的慢路径等待。
- 仍在进行中的慢路径等待。

## 5.2 自 parking 规则

在非 `SSC core` 上运行的线程，会在 `tick()` 中检查：

```text
wait_ns_window > run_ns_window / 10
```

也就是窗口内超过 10% 的 CPU 时间都耗在“边运行边等锁”上。

满足条件时：

- `tc->admitted = 0`
- `p->scx.slice = 0`

这样线程会尽快让出 CPU，并在下次 `enqueue()` 时进入 `SSC_DSQ`。

当前实现没有 owner 保护，因此这条规则完全基于等待占比，不区分线程是否刚从临界区出来，或是否即将获得锁。

## 5.3 `SSC core` 投票

每个 active `SSC core` 在窗口内都汇报一份快照：

- `last_run_ns`
- `last_wait_ns`

窗口有足够投票后，调度器计算：

```text
useful_run = max(ssc_vote_sum_run - ssc_vote_sum_wait, 0)
score = ssc_active_count * useful_run / ssc_vote_sum_run * 1024
```

直觉上：

- `ssc_active_count` 越大，说明更多 CPU 正在服务 `SSC_DSQ`。
- `useful_run / total_run` 越大，说明这些 CPU 花在“真正干活”上的比例越高。

调整规则：

- 连续两个窗口 `score` 比上一窗口更高：`ssc_active_count *= 2`
- 连续两个窗口 `score` 比最近一次生效值更低：`ssc_active_count /= 2`
- 最终把值 clamp 到 `[2, ssc_cpu_count]`

这是一种非常粗粒度但实现简单的乘法控制器。

## 5.4 任务放行规则

`dispatch()` 的行为：

- `stats_only_mode`：只消费 `READY_DSQ`。
- 否则，如果当前 CPU 是 active `SSC core` 且 `SSC_DSQ` 非空：
  - 先 `move_to_local(SSC_DSQ_ID)`。
- 之后无条件再 `move_to_local(READY_DSQ_ID)`。

这意味着：

- active `SSC core` 会优先服务被抑制线程。
- 其他 CPU 只处理 `READY_DSQ`。
- `SSC_DSQ` 的放行能力直接受 `ssc_active_count` 控制。

---

## 6. 拓扑与 CPU 选择

## 6.1 NUMA / socket 初始化

用户态初始化时会：

- 扫描 `/sys/devices/system/node/node*/cpulist`
- 生成 `cpu_to_node`
- 识别 CPU 最多的 node，写入 `dominant_node`
- 记录 `first_socket_cpus`

当前实现里真正用于 SSC 的并不是 `dominant_node`，而是：

- 把 `first_socket_cpus` 发布到 `ssc_cpu_list[]`
- 用 `ssc_cpu_rank[]` 记录每个 CPU 在该列表中的排名
- 令排名前 `ssc_active_count` 个 CPU 成为 active `SSC core`

所以当前 NUMA 支持更接近“固定 socket 内缩放 SSC 消费者”，而不是“按 local/remote 目标精确控并发”。

## 6.2 `select_cpu()`

当前 `select_cpu()` 只做两件事：

- 调用 `scx_bpf_select_cpu_dfl()` 取默认 hint。
- 如果找到了 idle CPU，则直接把任务插到 `SCX_DSQ_LOCAL`。

这个快路径不检查 `admitted`。因此 admission 不是在 `select_cpu()` 上硬阻断，而是靠后续的 `tick()` 自 parking 和 `enqueue()` 分流来收敛。

---

## 7. 运行模式

支持三种模式：

- 默认模式：
  - 加载 BPF，启用 `SSC_DSQ`、自 parking 和 `SSC core` 投票。
- `ACCORDIN_STATS_ONLY=1`
  - 仍加载 BPF 和记账，但不消费 `SSC_DSQ`，只保留统计路径。
- `ACCORDIN_DISABLE_BPF=1`
  - 完全不加载 BPF，只使用用户态 MCS-TAS 锁替换。

这三种模式构成了性能拆分实验的基线。

---

## 8. 当前实验快照

2026-03-16 在当前 40 CPU 机器上，使用 `bench/mutexbench` 对同一组参数做了两次四模式拆分复测：

- `threads=32`
- `critical_ns=350`
- `outside_ns=350`
- `duration_ms=3000`
- `warmup_duration_ms=1000`
- `repeats=3`
- `timeslice_extension=off`
  - 结果目录：`bench/mutexbench/results/cpu_breakdown_20260316T121714Z`
- `timeslice_extension=require`
  - 结果目录：`bench/mutexbench/results/cpu_breakdown_20260316T122557Z`
  - 该 run 以 `require` 模式完整跑通，说明当前机器上的 timeslice extension 可用；否则 benchmark 会直接失败。

`timeslice_extension=off` 结果如下：

| 模式 | 吞吐量 | ns/op | 平均等待 | handoff | steady CPU | steady cores |
|---|---:|---:|---:|---:|---:|---:|
| `mcs-tas` | 1.707M ops/s | 585.76 | 18.36us | 194.37ns | 3195.33% | 31.95 |
| `accordin_no_bpf` | 1.655M ops/s | 604.23 | 18.95us | 218.83ns | 3192.33% | 31.92 |
| `accordin_stats_only` | 1.610M ops/s | 620.96 | 19.48us | 227.39ns | 3198.67% | 31.99 |
| `accordin_full` | 0.567M ops/s | 1763.63 | 112.15us | 2536.51ns | 596.63% | 5.97 |

对应拆分：

- 总额外开销：`1177.88ns/op`
- 用户态开销：`18.47ns/op`
- BPF 统计开销：`16.73ns/op`
- 剩余调度器开销：`1142.68ns/op`

`timeslice_extension=require` 结果如下：

| 模式 | 吞吐量 | ns/op | 平均等待 | handoff | steady CPU | steady cores |
|---|---:|---:|---:|---:|---:|---:|
| `mcs-tas` | 1.645M ops/s | 607.99 | 19.05us | 211.03ns | 3195.75% | 31.96 |
| `accordin_no_bpf` | 1.624M ops/s | 615.93 | 19.33us | 228.39ns | 3196.89% | 31.97 |
| `accordin_stats_only` | 1.609M ops/s | 621.56 | 19.49us | 228.00ns | 3189.90% | 31.90 |
| `accordin_full` | 1.558M ops/s | 642.00 | 19.86us | 297.03ns | 642.13% | 6.42 |

对应拆分：

- 总额外开销：`34.00ns/op`
- 用户态开销：`7.93ns/op`
- BPF 统计开销：`5.63ns/op`
- 剩余调度器开销：`20.44ns/op`

当前可得出的结论：

- 纯用户态锁替换依然基本没有额外成本；`accordin_no_bpf` 与 `mcs-tas` 保持同一量级。
- 开启 timeslice extension 后，`accordin_full` 吞吐从 `0.567M` 提升到 `1.558M ops/s`，`ns/op` 从 `1763.63` 降到 `642.00`。
- 总额外开销从 `1177.88ns/op` 降到 `34.00ns/op`，其中剩余调度器开销从 `1142.68ns/op` 降到 `20.44ns/op`。
- full mode 仍然观察到明显 CPU limiting，但与基线相比的吞吐差距已经缩小到约 `5.30%`，不再是原来那种吞吐塌陷。

---

## 9. 已知限制

当前实现与早期方案相比，仍有几个明确限制：

- 没有 owner 保护：
  - 线程是否进入 `SSC_DSQ` 完全由等待占比决定。
- 没有 `stopping()`：
  - 记账和自 parking 依赖 `tick()`，响应延迟受 tick 周期影响。
- `select_cpu()` 的 idle fast path 会短暂绕过 admission：
  - 最终靠 `tick()` 再把线程压回 `SSC_DSQ`。
- 没有 seqcount：
  - BPF 对用户态上下文做单次 `bpf_probe_read_user()` 近似读取。
- 仍是全局 sched-ext：
  - 未实现 partial switch。
- NUMA 策略仍偏弱：
  - 当前只是固定一个 socket 作为 `SSC core` 候选池，没有 local/remote 精细控制。

---

## 10. 后续工作

下一步最值得做的事情是：

1. 给用户态导出增加 owner / in-critical-section 信号，避免把即将完成交接的线程过早压进 `SSC_DSQ`。
2. 把自 parking 判断从 `tick()` 挪到 `stopping()` 或与其结合，减少基于 tick 的延迟。
3. 重新审视 `wait > run/10` 这条阈值，确认它在 32 线程以上是否过于激进。
4. 让 `SSC core` 候选池基于真实 NUMA / LLC 拓扑，而不是固定第一颗 socket。
5. 在验证清楚控制律之前，再考虑把 global switch 收敛到 partial switch。
