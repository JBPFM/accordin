# lb_simple 实现状态文档

本文档对照 `design.md` 描述当前代码的实际实现情况，记录已完成功能、偏离点和已知缺口。

---

## 1. 整体架构

lb_simple 以 **cdylib（LD_PRELOAD 动态库）** 的形式交付：

- 库加载时（`.init_array` 构造函数）启动 eBPF 调度器并 attach。
- 通过 `dlsym(RTLD_NEXT, ...)` 拦截 pthread mutex / cond 接口，将其替换为 MCS-TAS 路径。
- BPF 程序以 **全局接管（非 SWITCH_PARTIAL）** 方式运行；admission 计数仅对注册了 `thread_ctx_addr_map` 的线程有效，从而避免系统其他任务稀释 wait ratio。

---

## 2. 锁侧（userspace）

### 2.1 MCS-TAS（`src/mcs_tas.rs`）

| 设计要求 | 实现状态 |
|---|---|
| TAS 快路径 | ✅ `lock()` 首先 `swap(true, Acquire)`，成功即返回 |
| MCS 慢路径队列等待 | ✅ 入队、等待 `waiting` 标志、MCS 传递逻辑均已实现 |
| `try_lock()` 使用 CAS | ✅ `compare_exchange(false, true, ...)` |
| 导出 `wait_ns_total` | ✅ 慢路径结束后累加 `wait_end - wait_start` |
| 导出 `state`（NONE / OWNER） | ✅ lock 后设 `ROLE_OWNER`，unlock 后设 `ROLE_NONE` |
| Seqcount 写侧保护 | ⚠️ `seq_begin()` / `seq_end()` 调用已**注释掉**；BPF 读侧仍有 seq 检查，但写侧不递增 seq，导致 BPF 端始终读到 seq=0（偶数），一致性保护实际上**未生效** |

- `unlock()` 先释放锁再清 role，短暂存在两线程都为 OWNER 的窗口，符合设计"保守性保护偏置"的意图。

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
    u64 wait_ns_total;
    u32 state;   // ROLE_NONE / ROLE_OWNER
    u32 seq;     // seqcount（写侧未使用）
};

struct task_scx_ctx {
    u64 last_wait_ns, run_start_ns, run_ns_window, wait_ns_window;
    u32 role, admitted, counted, counted_local;
    s32 last_node;
    u64 ssc_enter_ts;
};
```

与 design.md §7.1 对齐。`counted` / `counted_local` 是实现新增的两个辅助字段，用于跟踪任务是否已被计入 `active_local/remote`，避免重复计数。

### 3.2 Maps 与全局变量（`src/bpf/maps.bpf.h`）

| 变量 / Map | 值 | 说明 |
|---|---|---|
| `window_ns` | 4ms | 窗口长度，可运行时修改 |
| `p_high` | 0.35 (x1000=350) | 收缩阈值 |
| `p_low` | 0.20 (x1000=200) | 扩张阈值 |
| `ewma_alpha` | 0.20 (x1000=200) | EWMA 平滑系数 |
| `max_ssc_wait_ns` | 50ms | SSC 最大滞留时间（定义，部分执行，见 §3.5） |
| `min_ssc_dwell_ns` | 1ms | 最小驻留时间（定义，**未被 dispatch 使用**） |
| `H_persist` / `L_persist` | 2 | 持续窗口数阈值 |
| `target_local/remote` | 初始由拓扑决定 | 初始值等于各 NUMA 节点 CPU 数 |

所有控制参数均为 `volatile` 全局变量，支持从用户态运行时调整。

### 3.3 统计层（`src/bpf/stats.bpf.h`）

**per-task 记账（`account_task_activity`）：**
- `stopping()` 和 `tick()` 均调用此函数。
- 计算 `run_delta = now - run_start_ns`，累加到 `run_ns_window` / `agg_run_ns`。
- 通过 seqcount 读取用户态 `wait_ns_total`，计算 `wait_delta` 并累加到 `wait_ns_window` / `agg_wait_ns`。
- 注意：读取时同时更新了 `tc->role`（从 `uctx.state`）。

**窗口滚动（`try_advance_window`）：**
- 使用 cmpxchg 选出唯一 leader，避免多 CPU 竞争窗口。
- Leader 将 `agg_run_ns` / `agg_wait_ns` 原子置零并采样。
- EWMA 更新：`new_ewma = alpha * sample + (1 - alpha) * old_ewma`（x1000 定点）。
- 双阈值判断：`p_w > p_high` 累加 `consec_high`，`p_w < p_low` 累加 `consec_low`。
- 达到 `H_persist` 次时收缩（先减 remote，再减 local，local 下限 1）；达到 `L_persist` 次时扩张（只扩 local，上限为 `max_target_local`）。
- 将 9 个统计量写入 `stats_map` 供用户态读取。

**已知缺陷：**
- 扩张逻辑仅扩 local，从不扩 remote；设计要求"先扩本地，后扩远端"，远端扩张路径缺失。
- 收缩时 remote 下限为 0，不存在 `min_active_remote` 保护。

### 3.4 Admission（`src/bpf/admission.bpf.h`）

- `get_or_create_task_ctx()`：只为已注册 `thread_ctx_addr_map` 的线程创建上下文，新任务默认 `admitted=1`。
- `admit_task()`：设 `admitted=1, counted=1`，按 `is_local_node(last_node)` 选择递增 `active_local` 或 `active_remote`。
- `is_local_node()`：与 `dominant_node` 比较，`dominant_node` 在启动时固定为 CPU 数最多的 NUMA 节点。

**已知缺陷：**
- `dominant_node` 为**静态初始值**，不随活跃线程分布动态更新（设计 §11.1 要求动态选主节点）。

### 3.5 主回调（`src/bpf/main.bpf.c`）

**`select_cpu`：** 调用 `scx_bpf_select_cpu_dfl`，不做最终 admission，符合设计。

**`enqueue`：**
- OWNER → 直接入 READY_DSQ（若尚未 admitted 则先 admit）。
- admitted → 入 READY_DSQ。
- 其余 → 记录 `ssc_enter_ts`，入 SSC_DSQ。

**`dispatch`：**
- 若 SSC 有等待者且 `(al+ar) < (tl+tr)`（under target）：`move_to_local(SSC_DSQ_ID)`。
- Safety valve：若 `(al+ar) <= 0` 且 READY 为空：强制 `move_to_local(SSC_DSQ_ID)`。
- 否则：`move_to_local(READY_DSQ_ID)`。

**已知缺陷：**
- `min_ssc_dwell_ns` 未在 dispatch 中检查；进入 SSC 的任务在满足 under_target 时可立即被放行。
- `max_ssc_wait_ns` 未在 dispatch 中强制执行；safety valve 依赖 `active<=0 && READY==0` 条件，在高负载时可能长期不触发，watchdog 风险未完全消除（设计 §15.1）。
- `forced_release_cnt` 在 `dispatch` 中没有对应的递增操作。
- `scx_bpf_dsq_move_to_local` 只能移动 SSC 队头，无法按 NUMA 节点选择性释放（BPF API 限制）；设计 §14.3 要求的"同 NUMA 优先释放"实际无法实现。

**`running`：**
- 更新 `run_start_ns`。
- 读取当前 CPU 所在节点，若与 `counted_local` 不符则做 active 计数迁移。
- 若 `admitted=0` 或 `counted=0` 则调用 `admit_task()`（处理从 SSC 释放后的首次运行）。
- 从用户态读取 role（此处也读了一次，`stopping()` 路径的 `account_task_activity` 再读一次，存在双重读取）。

**`stopping`：**
- 调用 `account_task_activity` 记账。
- 调用 `try_advance_window`。
- 自主 parking 决策：`role != OWNER` 且 `admitted` 时，按 NUMA 判断 over_local / over_remote / over_total，满足则清 `admitted`，减 active 计数。

**`tick`：**
- 调用 `account_task_activity` + `try_advance_window`。
- 若 `(al+ar) > (tl+tr) || al > tl || ar > tr` 则置 `slice=0`，加速让出 CPU。

**`exit_task`：** 减 active 计数，清理 `task_ctx_map` 和 `thread_ctx_addr_map`。

**`init`：** 创建 READY_DSQ 和 SSC_DSQ，初始化 `window_start_ns`。

### 3.6 用户态读端

- `read_thread_ctx()`：读 seq1 → 读结构体 → 读 seq2，检查 seq1==seq2 且为偶数。
- **实际上写侧 seq 始终为 0（未递增），读侧始终通过检查**，seqcount 无保护效果。

---

## 4. 用户态初始化（`src/lib.rs`）

- 检测 NUMA 拓扑（`/sys/devices/system/node/nodeN/cpulist`），找出 CPU 最多的节点作为 dominant_node。
- `target_local = local_cpu_count`, `target_remote = remote_cpu_count`（初始宽松上限）。
- 写入 `cpu_to_node` BPF map，设置 `dominant_node` BSS 变量。
- 提取 `thread_ctx_addr_map` FD 存入 `THREAD_CTX_MAP_FD`，供 mutex_hook 注册线程时使用。

---

## 5. Benchmark 与测试

- `bench/mutexbench`：C++ 互斥锁基准测试。
- `bench/mutexbench_rust`：Rust 版本基准。
- `test/`：bpftrace 脚本，覆盖并发竞争者、缓存迁移、唤醒延迟等假设验证场景。

---

## 6. 与设计文档的差距汇总

| 设计要求 | 状态 | 说明 |
|---|---|---|
| MCS-TAS 基础锁 | ✅ 完成 | |
| try_lock 使用 CAS | ✅ 完成 | |
| 导出 wait_ns_total + role | ✅ 完成 | |
| Seqcount 写侧保护 | ❌ 缺失 | seq_begin/end 已注释 |
| per-task 窗口记账 | ✅ 完成 | |
| 单一 SSC DSQ | ✅ 完成 | |
| OWNER 保护 | ✅ 完成 | |
| 双阈值控制律 | ✅ 完成 | |
| EWMA wait ratio | ✅ 完成 | |
| NUMA 感知 active 计数 | ✅ 完成 | local/remote 分离计数 |
| SSC 最大滞留时间强制 | ⚠️ 不完整 | 变量定义但 dispatch 未检查 |
| SSC 最小驻留时间 | ❌ 缺失 | 变量定义但 dispatch 未使用 |
| 先收远端后收本地 | ✅ 完成 | try_advance_window 中已实现 |
| 先扩本地后扩远端 | ⚠️ 不完整 | 仅扩本地，远端扩张路径缺失 |
| dispatch 按 NUMA 选择性释放 | ❌ 不可行 | BPF API 只能移动 DSQ 队头 |
| dominant_node 动态更新 | ❌ 缺失 | 启动时固定 |
| forced_release_cnt 统计 | ❌ 不完整 | 计数器定义但未在释放路径递增 |
| slow_rate / runnable_pressure 统计 | ❌ 未实现 | 设计 §16.2 后续功能 |
| SWITCH_PARTIAL | ⚠️ 有意偏离 | 全局接管但 admission 仅对注册线程生效 |
