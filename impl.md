# lb_simple 实现状态文档

本文档记录当前代码的真实实现状态，并附上 2026-03-16 重新验证后的 benchmark 数据。

---

## 1. 实现摘要

`lb_simple` 当前是一个“用户态锁替换 + sched-ext 锁感知调度器”的组合实现：

- 以 `cdylib` 形式通过 `LD_PRELOAD` 拦截 `pthread_mutex_*` / `pthread_cond_*`。
- 用户态锁使用 MCS-TAS。
- BPF 调度器是全局 sched-ext，而不是 partial switch。
- 调度控制不是早期文档里的 `target_local / target_remote`，而是：
  - 线程在 `tick()` 中根据等待占比自 parking。
  - 被 parking 的线程重新入队时进入 `SSC_DSQ`。
  - 一组会动态伸缩的 `SSC core` 专门从 `SSC_DSQ` 消费任务。
- 支持两个实验模式：
  - `LB_SIMPLE_STATS_ONLY=1`
  - `LB_SIMPLE_DISABLE_BPF=1`

---

## 2. 用户态部分

## 2.1 MCS-TAS（`src/mcs_tas.rs`）

当前锁实现和文档中的旧版本相比，有三个关键变化：

1. 不再导出 `OWNER` / `role`。
2. 不再使用 seqcount。
3. 不再做 1/8 等待采样，而是每次慢路径都更新等待时间。

当前线程本地导出结构为：

```c
struct LockSchedThreadCtx {
    u64 wait_ns_total;
    u64 wait_start_ns;
    u64 wait_end_ns;
};
```

行为如下：

| 功能 | 当前状态 |
|---|---|
| TAS 快路径 | ✅ `swap(true, Acquire)` 成功即返回 |
| MCS 慢路径 | ✅ 入队、等待前驱、handoff 全部实现 |
| `try_lock()` | ✅ 使用 `compare_exchange(false, true, ...)` |
| 已完成等待累计 | ✅ 慢路径成功获取锁后累加到 `wait_ns_total` |
| 正在等待中的时间 | ✅ 通过 `wait_start_ns` / `wait_end_ns` 让 BPF 推断 |
| timeslice extension | ✅ contended lock 成功后申请，unlock 时归还 |

需要注意：

- 快路径不会更新 `wait_*` 字段。
- 慢路径开始时先写 `wait_start_ns`，完成获取后再写 `wait_end_ns`。
- BPF 侧把 `wait_end_ns < wait_start_ns` 当作“此刻仍在等锁”。

## 2.2 线程注册与 cond 桥接（`src/mutex_hook.rs`）

`mutex_hook.rs` 负责两件事：

1. 线程第一次进入拦截后的 `pthread_mutex_lock()` 时，把 `tid -> thread_ctx()` 注册到 `thread_ctx_addr_map`。
2. 在线程退出时通过 TLS guard 自动注销。

`pthread_cond_wait()` 仍然通过内部 `real_mutex + MCS-TAS` 双协议桥接，避免信号丢失。

---

## 3. BPF 侧

## 3.1 核心数据结构（`src/bpf/intf.h`）

当前导出头文件如下：

```c
struct lock_sched_thread_ctx {
  unsigned long long wait_ns_total;
  unsigned long long wait_start_ns;
  unsigned long long wait_end_ns;
};

struct task_scx_ctx {
  unsigned long long window_epoch;
  unsigned long long last_wait_ns;
  unsigned long long pending_wait_ns;
  unsigned long long run_start_ns;
  unsigned long long run_ns_window;
  unsigned long long wait_ns_window;
  unsigned int admitted;
  unsigned long long user_ctx_ptr;
};

struct ssc_vote_slot {
  unsigned long long epoch;
  unsigned long long last_run_ns;
  unsigned long long last_wait_ns;
};
```

这组结构已经不包含：

- `role`
- `last_node`
- `active_local / active_remote`
- `target_local / target_remote`

## 3.2 Maps 与全局变量（`src/bpf/maps.bpf.h`）

当前真正使用的 maps：

| Map | 用途 |
|---|---|
| `task_ctx_map` | per-task `task_scx_ctx` |
| `thread_ctx_addr_map` | `tid -> user_ctx_ptr` |
| `stats_map` | 导出统计量 |
| `cpu_to_node` | CPU -> NUMA node |
| `agg_percpu_map` | 每 CPU 的 `run_ns/wait_ns` 聚合 |
| `ssc_vote_slot_map` | 每个 active `SSC core` 的窗口投票槽 |

当前关键全局变量：

| 变量 | 默认值 | 说明 |
|---|---:|---|
| `ssc_vote_window_ns` | `2 * SCX_SLICE_DFL` | 控制窗口长度 |
| `ssc_active_count` | 2 | 当前活跃 `SSC core` 数 |
| `ssc_cpu_count` | 运行时写入 | `SSC` 候选 CPU 数 |
| `ssc_cpu_list[]` | 运行时写入 | 候选 CPU 列表 |
| `ssc_cpu_rank[]` | 运行时写入 | 每个 CPU 在候选列表中的排名 |
| `stats_only_mode` | 0 | 统计模式开关 |

还保留了 `dominant_node`、`forced_release_cnt`、调试计数等变量，但它们当前不是主控制环路的核心。

## 3.3 统计与投票（`src/bpf/stats.bpf.h`）

### `account_task_activity()`

当前记账完全由 `tick()` 驱动：

- 如果 `run_start_ns == 0`，先初始化后返回。
- 之后每次 `tick()`：
  - 用 `now - run_start_ns` 结算 `run_delta`
  - 读取用户态 `lock_sched_thread_ctx`
  - 结算已完成等待：`wait_ns_total - last_wait_ns`
  - 结算正在进行中的等待：`now - wait_start_ns`
  - 用 `pending_wait_ns` 避免重复累加
  - 同时写 task-local 和 per-CPU 聚合器

### `publish_ssc_core_vote()`

只有 active `SSC core` 才会上报投票。

投票统计：

- `ssc_vote_sum_run`
- `ssc_vote_sum_wait`
- `ssc_vote_publish_count`

评分函数：

```text
score = active_count * max(run - wait, 0) / run * 1024
```

控制规则：

- 连续两个窗口更好：`ssc_active_count <<= 1`
- 连续两个窗口更差：`ssc_active_count >>= 1`
- clamp 到 `[2, ssc_cpu_count]`

### `try_advance_window()`

当前实现中 `try_advance_window()` 是空函数，旧文档里那套 EWMA / `p_high` / `p_low` / `drought-safe` 逻辑已经不在代码里。

## 3.4 admission / 拓扑辅助（`src/bpf/admission.bpf.h`）

这里主要负责：

- `lookup_task_ctx()`
- `get_or_create_task_ctx()`
- `get_cpu_node()`
- `is_cpu_ssc_core()`
- `is_task_on_ssc_core()`

当前 `get_or_create_task_ctx()` 的默认行为是：

- 只有注册了 `thread_ctx_addr_map` 的线程才创建 `task_scx_ctx`
- 新 task 默认 `admitted = 1`

## 3.5 主回调（`src/bpf/main.bpf.c`）

### `select_cpu`

- 调用 `scx_bpf_select_cpu_dfl()`
- 如果返回 idle CPU，直接 `scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, ...)`

这里没有 admission 检查；真正的收敛发生在后续 `tick()` 和 `enqueue()`。

### `enqueue`

- `!tc || tc->admitted` -> `READY_DSQ`
- `tc && !tc->admitted` -> `SSC_DSQ`

### `dispatch`

- `stats_only_mode`：只消费 `READY_DSQ`
- 默认模式：
  - active `SSC core` 且 `SSC_DSQ` 非空 -> 先搬一个 `SSC_DSQ`
  - 然后再搬 `READY_DSQ`

### `running`

- 通过 `get_or_create_task_ctx()` 懒创建 task-local 状态
- 写入 `run_start_ns`

### `tick`

当前完整控制逻辑都在这里：

1. `maybe_rotate_ssc_vote_window(now)`
2. `account_task_activity(tc, pid, now)`
3. 如果当前 task 在 active `SSC core` 上：
   - `publish_ssc_core_vote()`
   - 根据评分增长/收缩 `ssc_active_count`
4. 否则：
   - 若 `wait_ns_window > run_ns_window / 10`
   - 则 `tc->admitted = 0` 且 `p->scx.slice = 0`

### `stopping`

当前未启用；旧实现文档里依赖 `stopping()` 的逻辑已经失效。

### `exit_task`

- 删除 `task_ctx_map`
- 删除 `thread_ctx_addr_map` 对应项

---

## 4. 用户态初始化（`src/lib.rs`）

初始化流程如下：

1. 探测 NUMA 拓扑。
2. 计算：
   - `dominant_node`
   - `first_socket_node`
   - `first_socket_cpus`
3. 把 `cpu_to_node` 写入 BPF map。
4. 把 `dominant_node`、`stats_only_mode` 写入 BSS。
5. 用 `first_socket_cpus` 填充：
   - `ssc_cpu_list[]`
   - `ssc_cpu_rank[]`
   - `ssc_cpu_count`
6. 把 `ssc_active_count` 初始化为 2。
7. 导出 `thread_ctx_addr_map` 的 `MapHandle`，供 `mutex_hook.rs` 做线程注册。

需要注意：

- 候选 `SSC core` 池来自第一颗 socket，而不是 `dominant_node`。
- 当前实现故意不设置 `SWITCH_PARTIAL`。

---

## 5. 2026-03-16 实验数据

本节只记录当前实现重新验证后的结果。

### 5.1 直接对比：`lb_simple` vs `mcs-tas`

命令：

```bash
/home/jz/.codex/skills/analyze-lb-simple-cpu-limit/scripts/run_bench_with_pidstat.sh \
  --repo /home/jz/Projects/lb_simple \
  -- \
  --locks lb_simple,mcs-tas \
  --sudo-mode auto \
  --threads 32 \
  --critical-ns 350 \
  --outside-ns 350 \
  --duration-ms 3000 \
  --warmup-duration-ms 1000 \
  --repeats 3
```

结果目录：

- `bench/mutexbench/results/cpu_limit_20260316T120057Z`

结果：

| 模式 | 吞吐量 | 等待 | handoff | steady CPU | steady cores |
|---|---:|---:|---:|---:|---:|
| `lb_simple` | 882,349 ops/s | 43.55us | 543.45ns | 457.04% | 4.57 |
| `mcs-tas` | 1,637,511 ops/s | 19.16us | 215.32ns | 3197.56% | 31.98 |

结论：

- 当前 full mode 明确观察到了 CPU limiting。
- 但吞吐仍比 `mcs-tas` 低约 `46.12%`。

### 5.2 四模式拆分（`timeslice_extension=off`）

命令：

```bash
/home/jz/.codex/skills/analyze-lb-simple-cpu-limit/scripts/run_breakdown_with_pidstat.sh \
  --repo /home/jz/Projects/lb_simple \
  --sudo-mode auto \
  -- \
  --threads 32 \
  --critical-ns 350 \
  --outside-ns 350 \
  --duration-ms 3000 \
  --warmup-duration-ms 1000 \
  --repeats 3
```

结果目录：

- `bench/mutexbench/results/cpu_breakdown_20260316T121714Z`

结果：

| 模式 | 吞吐量 | ns/op | 等待 | handoff | steady CPU | steady cores |
|---|---:|---:|---:|---:|---:|---:|
| `mcs-tas` | 1,707,193 ops/s | 585.76 | 18.36us | 194.37ns | 3195.33% | 31.95 |
| `lb_simple_no_bpf` | 1,654,998 ops/s | 604.23 | 18.95us | 218.83ns | 3192.33% | 31.92 |
| `lb_simple_stats_only` | 1,610,419 ops/s | 620.96 | 19.48us | 227.39ns | 3198.67% | 31.99 |
| `lb_simple_full` | 567,012 ops/s | 1763.63 | 112.15us | 2536.51ns | 596.63% | 5.97 |

拆分结论：

- 用户态锁替换开销：
  - `lb_simple_no_bpf - mcs-tas = 18.47ns/op`
  - 仍然很小，但当前 run 不再是负值
- BPF 统计路径开销：
  - `lb_simple_stats_only - lb_simple_no_bpf = 16.73ns/op`
  - 约占 full mode 总额外开销的 `1.42%`
- 完整调度路径剩余开销：
  - `lb_simple_full - lb_simple_stats_only = 1142.68ns/op`
  - 约占 total overhead 的 `97.01%`

当前代码最重的成本不在 interpose，也不在 BPF 读统计，而在完整的自 parking + `SSC_DSQ` + `SSC core` 控制路径。

### 5.3 四模式拆分（`timeslice_extension=require`）

命令：

```bash
/home/jz/.codex/skills/analyze-lb-simple-cpu-limit/scripts/run_breakdown_with_pidstat.sh \
  --repo /home/jz/Projects/lb_simple \
  --sudo-mode auto \
  -- \
  --timeslice-extension require \
  --threads 32 \
  --critical-ns 350 \
  --outside-ns 350 \
  --duration-ms 3000 \
  --warmup-duration-ms 1000 \
  --repeats 3
```

结果目录：

- `bench/mutexbench/results/cpu_breakdown_20260316T122557Z`

结果：

| 模式 | 吞吐量 | ns/op | 等待 | handoff | steady CPU | steady cores |
|---|---:|---:|---:|---:|---:|---:|
| `mcs-tas` | 1,644,751 ops/s | 607.99 | 19.05us | 211.03ns | 3195.75% | 31.96 |
| `lb_simple_no_bpf` | 1,623,566 ops/s | 615.93 | 19.33us | 228.39ns | 3196.89% | 31.97 |
| `lb_simple_stats_only` | 1,608,856 ops/s | 621.56 | 19.49us | 228.00ns | 3189.90% | 31.90 |
| `lb_simple_full` | 1,557,635 ops/s | 642.00 | 19.86us | 297.03ns | 642.13% | 6.42 |

拆分结论：

- 用户态锁替换开销：
  - `lb_simple_no_bpf - mcs-tas = 7.93ns/op`
- BPF 统计路径开销：
  - `lb_simple_stats_only - lb_simple_no_bpf = 5.63ns/op`
- 完整调度路径剩余开销：
  - `lb_simple_full - lb_simple_stats_only = 20.44ns/op`

和 `timeslice_extension=off` 相比：

- `lb_simple_full` 吞吐从 `567,012` 提升到 `1,557,635 ops/s`
- `lb_simple_full` 的 `ns/op` 从 `1763.63` 降到 `642.00`
- 总额外开销从 `1177.88ns/op` 降到 `34.00ns/op`
- 剩余调度器开销从 `1142.68ns/op` 降到 `20.44ns/op`

`require` 模式完整跑通也说明当前机器上的 timeslice extension 可用；否则 benchmark 会在初始化或 yield 路径直接退出。

---

## 6. 当前实现与设计文档的差距

| 项目 | 状态 | 说明 |
|---|---|---|
| MCS-TAS 基础锁 | ✅ | 已实现 |
| wait 时间导出 | ✅ | 使用 `wait_ns_total/start/end` |
| owner/role 导出 | ❌ | 当前没有 |
| seqcount 一致性保护 | ❌ | 当前没有 |
| `stopping()` 记账 | ❌ | 当前未启用 |
| `tick()` 自 parking | ✅ | 当前主控制路径 |
| `SSC_DSQ` | ✅ | 已实现 |
| 动态 `ssc_active_count` | ✅ | 通过窗口投票按 2x/0.5x 调整 |
| `target_local/target_remote` 控制 | ❌ | 当前已废弃 |
| 精细 NUMA admission | ❌ | 当前只固定第一颗 socket 作为 `SSC` 候选池 |
| `LB_SIMPLE_STATS_ONLY` | ✅ | 已实现 |
| `LB_SIMPLE_DISABLE_BPF` | ✅ | 已实现 |
| partial switch | ❌ | 当前仍是全局 sched-ext |

---

## 7. 已知问题与后续工作

## 7.1 full mode 吞吐回退仍大

当前 32 线程 / 350ns / 350ns 参数下：

- steady CPU 从约 32 核降到 4 核
- 吞吐也同步掉到约 0.78M 到 0.88M ops/s

这说明当前阈值和放行策略仍然偏激进。

## 7.2 没有 owner 保护

线程是否被压进 `SSC_DSQ` 只取决于等待占比，可能把“即将完成锁交接”的线程也压掉。

## 7.3 admission 收敛依赖 `tick()`

由于没有 `stopping()`，记账和自 parking 都依赖 `tick()`，可能导致：

- admission 收敛滞后
- 线程在被重新压制前已经多跑了一个或多个 tick

## 7.4 NUMA 信息还没有真正进入控制律

当前只有：

- `cpu_to_node`
- `dominant_node`
- `first_socket_cpus -> ssc_cpu_list`

还没有：

- local/remote 独立目标
- NUMA-aware release/park
- 基于 LLC 的 READY/SSC 分层
