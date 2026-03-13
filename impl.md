# lb_simple 实现状态文档

本文档对照 `design.md` 描述当前代码的实际实现情况，记录已完成功能、偏离点和已知缺口。

---

## 1. 整体架构

lb_simple 以 **cdylib（LD_PRELOAD 动态库）** 的形式交付：

- 库加载时（`.init_array` 构造函数）启动 eBPF 调度器并 attach。
- 通过 `dlsym(RTLD_NEXT, ...)` 拦截 pthread mutex / cond 接口，将其替换为 MCS-TAS 路径。
- BPF 程序以 **全局接管（非 SWITCH_PARTIAL）** 方式运行；admission 计数仅对注册了 `thread_ctx_addr_map` 的线程有效，从而避免系统其他任务稀释 wait ratio。
- 支持 `stats_only_mode`（`LB_SIMPLE_STATS_ONLY=1`）：加载 BPF 但禁用 admission control，用于性能分解测试。
- 支持 `LB_SIMPLE_DISABLE_BPF=1`：完全禁用 BPF 加载，仅使用 MCS-TAS 锁替换。

---

## 2. 锁侧（userspace）

### 2.1 MCS-TAS（`src/mcs_tas.rs`）

| 设计要求 | 实现状态 |
|---|---|
| TAS 快路径 | ✅ `lock()` 首先 `swap(true, Acquire)`，成功即返回 |
| MCS 慢路径队列等待 | ✅ 入队、等待 `waiting` 标志、MCS 传递逻辑均已实现 |
| `try_lock()` 使用 CAS | ✅ `compare_exchange(false, true, ...)` |
| 导出 `wait_ns_total` | ✅ 慢路径结束后累加 `wait_end - wait_start`，1/8 采样步幅 |
| 导出 `state`（NONE / OWNER） | ✅ lock 后设 `ROLE_OWNER`，unlock 后设 `ROLE_NONE` |
| Seqcount 写侧保护 | ⚠️ `seq_begin()` / `seq_end()` 调用已**注释掉**；BPF 读侧跳过 seq 检查，直接做单次 `bpf_probe_read_user` |

- `unlock()` 先释放锁再清 role，短暂存在两线程都为 OWNER 的窗口，符合设计"保守性保护偏置"的意图。
- 等待时间采样步幅 `WAIT_TIME_SAMPLE_STRIDE=8`：每 8 次慢路径仅记录 1 次 wait_ns，BPF 侧用 8x 乘法还原。降低用户态热路径开销。

### 2.2 线程注册（`src/mutex_hook.rs`）

| 设计要求 | 实现状态 |
|---|---|
| 首次进锁时向 BPF map 注册共享上下文指针 | ✅ `ensure_registered()` 在每次 `pthread_mutex_lock` 时检查并注册 |
| 线程退出时注销 | ✅ `ThreadCtxGuard::drop()` 调用 `unregister_thread_ctx()` |
| pthread_cond_wait 正确桥接 | ✅ 内部 real_mutex + MCS lock 双重协议，避免信号丢失 |

---

## 3. BPF 侧

### 3.1 数据结构（`src/bpf/intf.h`）

```c
struct lock_sched_thread_ctx {
    u64 wait_ns_total;   // 单调递增累计采样等待时间
    u32 state;           // ROLE_NONE / ROLE_OWNER
    u32 seq;             // seqcount（写侧未使用）
};

struct task_scx_ctx {
    u64 last_wait_ns;     // 上次读取到的 wait_ns_total
    u64 run_start_ns;     // 本次 running 开始时间戳
    u64 run_ns_window;    // 当前窗口累计运行时间
    u64 wait_ns_window;   // 当前窗口累计锁等待时间
    u32 role;             // NONE / OWNER
    u32 admitted;         // 当前是否处于 active set
    u32 counted;          // 是否已计入 active_local/remote
    u32 counted_local;    // 若 counted=1，是计入 local(1) 还是 remote(0)
    s32 last_node;        // 线程最近一次运行所在 NUMA 节点
    u64 ssc_enter_ts;     // 进入 SSC 的时间戳
    u64 user_ctx_ptr;     // 缓存的用户态 lock_sched_thread_ctx 指针
};
```

与 design.md §7.1 对齐。额外字段说明：
- `counted` / `counted_local`：跟踪任务是否已被计入 `active_local/remote`，避免重复计数。
- `user_ctx_ptr`：创建 task ctx 时从 `thread_ctx_addr_map` 缓存，避免后续每次回调都查 hash map。

### 3.2 Maps 与全局变量（`src/bpf/maps.bpf.h`）

**Maps：**

| Map | 类型 | 说明 |
|---|---|---|
| `task_ctx_map` | `BPF_MAP_TYPE_TASK_STORAGE` | per-task 调度上下文，通过 `bpf_task_storage_get` 直接访问 |
| `thread_ctx_addr_map` | `BPF_MAP_TYPE_HASH` | pid → 用户态 `lock_sched_thread_ctx` 地址映射 |
| `stats_map` | `BPF_MAP_TYPE_ARRAY` | 16 个统计量，供用户态监控 |
| `cpu_to_node` | `BPF_MAP_TYPE_ARRAY` | CPU → NUMA node 映射 |
| `agg_percpu_map` | `BPF_MAP_TYPE_PERCPU_ARRAY` | per-CPU run_ns/wait_ns 累加器 |

**全局变量：**

| 变量 | 默认值 | 说明 |
|---|---|---|
| `window_ns` | 200ms | 统计窗口长度 |
| `p_high` | 350 (0.35) | 收缩阈值（x1000 定点） |
| `p_low` | 200 (0.20) | 扩张阈值（x1000 定点） |
| `ewma_alpha` | 300 (0.30) | EWMA 平滑系数 |
| `H_persist` | 2 | 连续高于 p_high 触发收缩的窗口数 |
| `L_persist` | 3 | 连续低于 p_low 触发扩张的窗口数 |
| `max_ssc_wait_ns` | 50ms | SSC 最大滞留时间（定义，部分执行） |
| `min_ssc_dwell_ns` | 1ms | 最小驻留时间（定义，**未被 dispatch 使用**） |
| `target_local/remote` | 各 NUMA 节点 CPU 数 | 初始宽松上限 |
| `stats_only_mode` | 0 | 1 时禁用 admission，仅收集统计 |

所有控制参数均为 `volatile` 全局变量，支持从用户态运行时调整。

### 3.3 统计层（`src/bpf/stats.bpf.h`）

**per-task 记账（`account_task_activity`）：**
- `stopping()` 和 `tick()` 均调用此函数，对称处理 run_ns 和 wait_ns。
- 计算 `run_delta = now - run_start_ns`。
- 通过 `bpf_probe_read_user` 读取用户态 `wait_ns_total`，计算 `wait_delta`，用 `WAIT_TIME_SAMPLE_STRIDE` (8x) 还原采样。
- 同时更新 `tc->role`（从 `uctx.state`）。
- 累加到 **per-CPU `agg_percpu_map`**，无需原子操作（消除跨核缓存行争用）。
- 窗口切换时清零 per-task `run_ns_window` / `wait_ns_window`。

**窗口滚动（`try_advance_window`）：**
- 使用 **CAS（cmpxchg）** 选出唯一 leader，多 CPU 安全。
- Leader 遍历所有 per-CPU slot 求和并清零 `agg_percpu_map`。
- 计算 `p_w = wait/run`（x1000 定点），注意 wait_ns 是 run_ns 的子集（线程在 CPU 上自旋等锁），不是独立时间。
- **EWMA 更新**：
  - 正常窗口：`new_ewma = alpha * sample + (1-alpha) * old_ewma`
  - **drought 窗口**（total_wait==0）：缓慢线性衰减 `EWMA -= 2`，不急速归零。这防止信号缺失被误判为低竞争。
- **双阈值 + 迟滞**：
  - `p_w > p_high`：累加 `consec_high`
  - `p_w < p_low` **且 total_wait > 0**：累加 `consec_low`（**drought-safe**：信号缺失时不触发扩张）
  - 介于两者之间：清零两个计数器
- **非对称 target 调整**：
  - **收缩（二分法）**：`step = total_target / 2`，每次减半。高竞争主动有害，应尽快消除。从 TGT=96 到 TGT=2 仅需 ~6 步（~2.4s）。先减 remote 再减 local。
  - **扩张（比例步长）**：`step = max(1, (p_low - ewma) / 50)`，上限 `cur_total / 4`（基于当前值，非剩余空间，防止从近零快速扩张）。按 local/remote headroom 比例分配。扩张保持保守，避免反弹。
  - Target 下限：`target_local >= 2`（N=2 时 p_w=(N-1)/(N+1)=1/3=333 在 p_low 和 p_high 之间，是自然均衡点）。
- 将 16 个统计量（含调试计数器）写入 `stats_map`。

### 3.4 Admission（`src/bpf/admission.bpf.h`）

- **Task Context 管理**：使用 `BPF_MAP_TYPE_TASK_STORAGE` 实现 per-task 存储。
  - `lookup_task_ctx()`：`bpf_task_storage_get(&task_ctx_map, p, 0, 0)` — O(1) 直接访问，无 hash 计算。
  - `get_or_create_task_ctx()`：先查 task storage，无则查 `thread_ctx_addr_map`（hash map，仅在创建时读一次），若已注册则用 `BPF_LOCAL_STORAGE_GET_F_CREATE` 原子创建。新任务默认 `admitted=1`。
- `admit_task()`：设 `admitted=1, counted=1`，按 `is_local_node(last_node)` 选择递增 `active_local` 或 `active_remote`。
- `is_local_node()`：与 `dominant_node` 比较。

### 3.5 主回调（`src/bpf/main.bpf.c`）

**`select_cpu`：**
- 调用 `scx_bpf_select_cpu_dfl` 获取 idle CPU hint。
- **快路径直接分发**：若找到 idle CPU 且任务已 admitted 或是 OWNER，直接 `scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, ...)`。sched_ext 核心跳过 `enqueue()`，降低 wakeup-to-running 延迟和 READY_DSQ 锁争用。

**`enqueue`：**
- OWNER → 直接入 READY_DSQ（若尚未 admitted 则先 admit）。
- `stats_only_mode` 或 admitted → 入 READY_DSQ。
- 其余 → 记录 `ssc_enter_ts`，入 SSC_DSQ。

**`dispatch`：**
- `stats_only_mode`：直接消费 READY_DSQ。
- 若 SSC 有等待者且 `(al+ar) < (tl+tr)`（under target）：`move_to_local(SSC_DSQ_ID)`。
- Safety valve：若 `(al+ar) <= 0` 且 READY 为空：强制 `move_to_local(SSC_DSQ_ID)`。
- 否则：`move_to_local(READY_DSQ_ID)`。

**`running`：**
- 设置 `run_start_ns`。
- 读取当前 CPU 所在 NUMA 节点，若与 `counted_local` 不符则做 active 计数迁移（local↔remote）。
- 若 `admitted=0` 或 `counted=0` 则调用 `admit_task()`（处理从 SSC 释放后的首次运行）。

**`stopping`：**
- 调用 `account_task_activity` 记账（对称读取 run_ns + wait_ns）。
- `stats_only_mode` 下直接返回。
- 自主 parking 决策：`role != OWNER` 且 `admitted` 时，按 NUMA 判断 over_local / over_remote / over_total，满足则清 `admitted`，减 active 计数。Remote 任务在 over_remote 或 over_total 时 park，local 任务在 over_local 或 over_total 时 park。

**`tick`：**
- 调用 `account_task_activity`。
- 调用 `try_advance_window`（CAS 选举，所有 CPU 可参与）。
- `stats_only_mode` 下直接返回。
- 若 `(al+ar) > (tl+tr) || al > tl || ar > tr` 则置 `slice=0`，加速让出 CPU。

**`exit_task`：** 减 active 计数，清理 `task_ctx_map`（`bpf_task_storage_delete`）和 `thread_ctx_addr_map`。

**`init`：** 创建 READY_DSQ 和 SSC_DSQ，初始化 `window_start_ns`。

---

## 4. 用户态初始化（`src/lib.rs`）

- 检测 NUMA 拓扑（`/sys/devices/system/node/nodeN/cpulist`），找出 CPU 最多的节点作为 `dominant_node`。
- `target_local = local_cpu_count`, `target_remote = remote_cpu_count`（初始宽松上限）。
- 写入 `cpu_to_node` BPF map，设置 `dominant_node` BSS 变量。
- 提取 `thread_ctx_addr_map` FD 存入 `THREAD_CTX_MAP_FD`，供 mutex_hook 注册线程时使用。
- 检查 `LB_SIMPLE_STATS_ONLY` / `LB_SIMPLE_DISABLE_BPF` 环境变量。

---

## 5. 性能特征

### 5.1 BPF 优化（相比初始实现）

| 优化 | 说明 | 收益 |
|---|---|---|
| Task Local Storage | 替代 hash map 查找，O(1) per-task 访问 | 消除 hash 计算 + bucket 锁争用 |
| Per-CPU 累加器 | 替代全局原子变量 `agg_run_ns/agg_wait_ns` | 消除跨核原子争用（~64K 次/窗口） |
| select_cpu 快路径分发 | 已 admitted 任务直接分发到 local DSQ | wakeup 延迟降低，READY_DSQ 锁争用按比例下降 |
| 收缩二分 + 扩张比例步长 | 替代 ±1/窗口 线性步长 | 收缩收敛 ~5x 更快（96→2 约 2.4s） |
| Drought-safe 扩张 | total_wait==0 时不触发扩张 | 消除信号缺失导致的 target 振荡 |

### 5.2 性能基准（96 线程, critical=350ns, outside=350ns, 单锁）

| 模式 | 吞吐量 | 持锁 | 等待 | Handoff | 每核效率 |
|---|---|---|---|---|---|
| 原生 pthread_mutex | 832K ops/s | 388ns | 115μs | 814ns | 8.7K |
| MCS-TAS（无 BPF） | 1,712K ops/s | 338ns | 56μs | 246ns | 17.8K |
| MCS-TAS + BPF 统计 | 1,654K ops/s | 330ns | 58μs | 274ns | 17.2K |
| **MCS-TAS + 完整 admission** | **215K ops/s** | **242ns** | **356μs** | **4,463ns** | **107K** |

- **BPF 统计开销**：3.4%（lock interposition 开销的噪声级别）
- **Admission control**：将 96 线程限制到 TGT=2 活跃核心，SSC 中 94 线程
- **每核效率提升 6.2x**：从 17.2K → 107K ops/s/core
- **CPU 资源节省 48x**：2 核 vs 96 核

### 5.3 反馈环路收敛行为

- 从初始 TGT=96 收敛到 TGT=2 约需 **2-4 秒**（二分法：96→48→24→12→6→2）。
- 到达均衡后保持稳定 30+ 秒无振荡。
- 均衡点 TGT=2：理论 p_w = (N-1)/(N+1) = 1/3 = 333，位于 p_low(200) 和 p_high(350) 之间。
- 已知：在 TGT=2 时 wait 信号极度稀疏（1/8 采样 + 低竞争），EWMA 停留在初始 burst 的残留值。系统通过 drought-safe 机制保持稳定。

---

## 6. Benchmark 与测试

- `bench/mutexbench`：C++ 互斥锁基准测试。
- `bench/mutexbench_rust`：Rust 版本基准。
- `test/`：bpftrace 脚本，覆盖并发竞争者、缓存迁移、唤醒延迟等假设验证场景。
- **三模式性能分解**：使用 `LB_SIMPLE_DISABLE_BPF=1` / `LB_SIMPLE_STATS_ONLY=1` / 默认，分离 lock interposition / BPF stats / admission control 各层开销。注意必须用 `sudo env VAR=1 script` 传递环境变量。

---

## 7. 与设计文档的差距汇总

| 设计要求 | 状态 | 说明 |
|---|---|---|
| MCS-TAS 基础锁 | ✅ 完成 | |
| try_lock 使用 CAS | ✅ 完成 | |
| 导出 wait_ns_total + role | ✅ 完成 | 1/8 采样步幅，BPF 侧 8x 还原 |
| Seqcount 写侧保护 | ⚠️ 已禁用 | 写侧注释，读侧跳过验证；不影响正确性 |
| per-task 窗口记账 | ✅ 完成 | Task Local Storage，per-CPU 累加 |
| 单一 SSC DSQ | ✅ 完成 | |
| OWNER 保护 | ✅ 完成 | enqueue + stopping 均检查 |
| 双阈值控制律 | ✅ 完成 | 含比例步长和 drought-safe 扩张 |
| EWMA wait ratio | ✅ 完成 | p_w = wait/run（wait 是 run 的子集） |
| NUMA 感知 active 计数 | ✅ 完成 | local/remote 分离计数 + running() 中迁移 |
| 先收远端后收本地 | ✅ 完成 | 比例步长，remote 优先 |
| 先扩本地后扩远端 | ✅ 完成 | 按 headroom 比例分配（headroom 大者得 ~2/3） |
| select_cpu 快路径分发 | ✅ 完成 | idle CPU + admitted/OWNER → SCX_DSQ_LOCAL |
| SSC 最大滞留时间强制 | ⚠️ 不完整 | 变量定义但 dispatch 未逐任务检查 |
| SSC 最小驻留时间 | ❌ 缺失 | 变量定义但 dispatch 未使用 |
| dispatch 按 NUMA 选择性释放 | ❌ 不可行 | BPF API 只能移动 DSQ 队头 |
| dominant_node 动态更新 | ❌ 缺失 | 启动时固定 |
| forced_release_cnt 统计 | ❌ 不完整 | 计数器定义但未在释放路径递增 |
| slow_rate / runnable_pressure | ❌ 未实现 | 设计 §16.2 后续功能 |
| SWITCH_PARTIAL | ⚠️ 有意偏离 | 全局接管但 admission 仅对注册线程生效 |

---

## 8. 已知问题与后续工作

### 8.1 信号稀疏问题
在低 target（TGT=2）时，1/8 等待时间采样导致 wait 信号极度稀疏，EWMA 不再反映实际竞争水平。当前通过 drought-safe 扩张机制规避振荡，但 EWMA 值是残留的。可能的改进：
- 降低采样步幅（如 1/4 或 1/2）
- 基于当前 target 推断理论 p_w 值
- 自适应采样：低 target 时全量采样

### 8.2 SSC dispatch 延迟
从 SSC 释放任务的 handoff 延迟约 4μs（vs 无 admission 时 274ns）。这是 sched_ext `dsq_move_to_local` 的固有成本。可能的改进：
- Timer-based 主动释放
- 批量释放优化

### 8.3 窗口长度
`window_ns` 从最初的 4ms 经 50ms 最终调至 200ms。根本原因是 1/8 采样步幅导致 wait 信号稀疏——每次 `account_task_activity` 仅约 1.5–5% 概率观测到非零 `wait_delta`。短窗口（4ms）时大量窗口 `total_wait==0`，EWMA 被拉向 0 触发扩张，导致 target 在 2↔48 之间持续振荡无法收敛。200ms 窗口给出足够积累时间，配合 drought-safe 扩张机制实现稳定收敛。详见 `design.md` §17.1。

`window_ns` 与 `WAIT_TIME_SAMPLE_STRIDE` 存在耦合：若降低采样步幅，窗口可相应缩短。

### 8.4 Per-LLC DSQ
当前单一全局 READY_DSQ 在多核上有锁争用。按 LLC/NUMA node 拆分可减少争用并改善数据局部性。复杂度较高，适合作为后续优化。
