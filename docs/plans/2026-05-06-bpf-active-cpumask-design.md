# Move dynamic CPU affinity enforcement into BPF

Date: 2026-05-06
Status: design

## Background

`accordin_shared::cpu_affinity` 维护一份"活跃 CPU 集合"。运行期 `lock_stats` 根据
临界区/非临界区时间比例调用 `update_dynamic_cpu_count(N)`，把活跃 CPU 数缩到
NUMA 排序后的前 N 个。当前实现在用户态用 `sched_setaffinity` 逐 TID 应用：

```
update_dynamic_cpu_count(N)
  └─ apply_process_configured_affinity
       └─ for tid in /proc/self/task: sched_setaffinity(tid, mask)
```

这条路径与 sched_ext BPF 调度器之间存在竞态：

1. `sched_setaffinity` 是逐 TID、非事务性的。在迭代 `/proc/self/task` 期间，
   线程 A 已经更新、线程 B 还没更新；BPF `select_cpu`/`enqueue` 看到的
   `p->cpus_ptr` 处于"半新半旧"状态。
2. 迭代过程中新 fork 出的线程从父线程拷贝旧 mask，跳过本轮更新。
3. BPF `task_ctx->admission_cpu` 缓存了一个具体 CPU。下一次调度时该 CPU 可能
   刚被剔除，但 `clear_invalid_admission_cpu` 依赖的 `cpus_ptr` 同样存在更新
   时间窗，可能仍报告为"允许"。
4. 即使 BPF 已经把任务投递到 `SCX_DSQ_LOCAL_ON | cpu`，紧接着 userspace 把
   该 cpu 从 mask 中剔除，sched-ext 可能因 cpus_ptr 不再包含该 cpu 而触发
   错误。

需要把"动态收缩活跃 CPU 集合"的执行从用户态迁到 BPF：BPF 持有一份权威的
活跃 mask，所有调度决策（`select_cpu` / `enqueue` / `admission_cpu` 校验等）
统一以这份 mask 为准；用户态只通过一次原子调用通知 BPF 新 mask，无中间窗。

## Scope

In-scope:
- 用户态 `update_dynamic_cpu_count` 不再调 `sched_setaffinity`，改为一次性
  把目标 mask 推到 BPF。
- BPF 维护 `active_cpumask`，所有调度决策（`enqueue` / `dispatch` /
  `select_cpu` / `admission_cpu` 校验等）统一以这份 mask 为准；用户态
  只通过一次原子调用通知 BPF 新 mask，无中间窗。
- 锁竞争负载下热路径主要是 `enqueue` 和 `dispatch`：前者通过
  `task_cpu_allowed` 级联拒绝非活跃 cpu，后者通过显式 early return
  让非活跃 cpu 不再向自己 LOCAL 拉任务。
- 缩容时把已挂在被剔除 cpu 的 `inactive_dsq` 上的任务排空到
  `READY_DSQ_ID`，确保即使 `steal_inactive` 扫描窗口
  (`INACTIVE_STEAL_SCAN`) 没扫到、或缩容后剩余 cpu 太少没人偷，残留任务
  仍会被搬走。
- `init_from_env` 维持原行为：当前线程一次性 `sched_setaffinity`（无竞态）。
  此外把同样的初始 mask 推到 BPF，作为 BPF `active_cpumask` 的初值。

Out-of-scope:
- 强制把已经在非活跃 cpu LOCAL_ON 队列上的任务驱逐出去：让其跑完当前 slice，
  下次 enqueue 自然重路由（设计选择"自然迁移"）。
- 改 admission 信令、stats、`task_ctx_map`、`thread_ctx_addr_map` 等其它现有
  通路。这次改动只新增"用户态 → BPF 推 mask"一条控制路径。

## Architecture

### Data flow

```
用户态 cpu_affinity.rs                          BPF (main.bpf.c)
─────────────────────                          ─────────────────
update_dynamic_cpu_count(N)
  ├─ active_cpu_count.swap(N)
  └─ BpfActiveCpusSink.push(wanted[MAX_CPUS])
        │
        └─ libbpf bpf_prog_test_run_opts ────► SEC("syscall")
                                                accordin_set_active_cpus
                                                  ├─ rcu_read_lock
                                                  │  for each cpu:
                                                  │    bpf_cpumask_set/clear_cpu
                                                  │  rcu_read_unlock
                                                  └─ for cpu in newly-inactive:
                                                       drain inactive_dsq → READY_DSQ
                                                       scx_bpf_kick_cpu(cpu, 0)

调度热路径
─────────
select_cpu / enqueue / pick_allowed_cpu / clear_invalid_admission_cpu
  └─ task_cpu_allowed(p, cpu) := bpf_cpumask_test_cpu(cpu, p->cpus_ptr)
                                && cpu_is_active(cpu)
```

### Invariants

- 一旦 syscall prog 返回，后续任何 BPF 回调都不会把任务投递到非活跃 cpu 的
  `SCX_DSQ_LOCAL_ON` 或 `inactive_dsq`。
- syscall prog 在 RCU 读侧批量执行 set/clear，对外读者要么看见旧 mask、要么
  看见新 mask（不会观察到"半完成"的半新半旧 mask）。这是保证调度决策与
  mask 一致的关键。
- LOCAL_ON 已投递的任务跑完当前 slice、再次进入 `enqueue` 时就会用新 mask 重
  路由。短期内（≤ 一个 slice）非活跃 cpu 上仍会运行任务，可接受。
- BPF `active_cpumask` 为 `NULL` 时（极早期）所有 cpu 视为活跃（fail open），
  防止启动期空 mask 卡死调度。

## BPF side

### `active_cpumask` 全局变量

```c
/* main.bpf.c top level */
struct bpf_cpumask __kptr *active_cpumask;
```

不需要包 map：BPF `__kptr` 现在合法地用作全局变量（`.data` 段隐式即一个单
entry array map），sched_ext 主线（如 `scx_central`）已是这种写法。

### Init / exit

```c
s32 BPF_STRUCT_OPS_SLEEPABLE(accordin_init) {
    struct bpf_cpumask *m;

    /* DSQ 创建保持原状 */

    m = bpf_cpumask_create();
    if (!m)
        return -ENOMEM;
    /* 默认全 0；用户态在 attach 之前会推一次完整初始 mask */
    m = bpf_kptr_xchg(&active_cpumask, m);
    if (m)
        bpf_cpumask_release(m);
    return 0;
}

void BPF_STRUCT_OPS(accordin_exit, struct scx_exit_info *ei) {
    struct bpf_cpumask *m = bpf_kptr_xchg(&active_cpumask, NULL);
    if (m)
        bpf_cpumask_release(m);
    UEI_RECORD(uei, ei);
}
```

### 读取助手

```c
static __always_inline bool cpu_is_active(__u32 cpu) {
    struct bpf_cpumask *mask = active_cpumask;
    if (!mask)
        return true;          /* fail open during early boot */
    return bpf_cpumask_test_cpu(cpu, (const struct cpumask *)mask);
}
```

### 热路径覆盖

**前提**：本调度器目标是用户态锁竞争场景，`accordin_select_cpu` 命中率低；
绝大多数调度决策在 `accordin_enqueue` 与 `accordin_dispatch`。两条路径必须
都显式覆盖 active mask，不能只靠 `task_cpu_allowed` 级联。

#### `task_cpu_allowed` 改为交集

```c
static __always_inline bool task_cpu_allowed(struct task_struct *p, __u32 cpu) {
    if (cpu >= MAX_CPUS)
        return false;
    if (!bpf_cpumask_test_cpu(cpu, p->cpus_ptr))
        return false;
    return cpu_is_active(cpu);
}
```

这覆盖 `accordin_enqueue` 路径上的所有 cpu 选择点：`pick_allowed_cpu`、
`requested_cpu`、`clear_invalid_admission_cpu`、以及 `enqueue` 中
`task_ctx->admission_cpu < MAX_CPUS && task_cpu_allowed(...)` 的检查全部
自动拒绝非活跃 cpu。`select_cpu` 同理覆盖；额外注意
`scx_bpf_select_cpu_dfl` 命中 `is_idle` 的直派要复检 `cpu_is_active(cpu)`，
不成立则回退到 `pick_allowed_cpu` 重选并跳过 `SCX_DSQ_LOCAL` 直派。

#### `accordin_dispatch` 早退（关键）

dispatch 自己不调 `task_cpu_allowed`，必须独立加判断：

```c
void BPF_STRUCT_OPS(accordin_dispatch, s32 cpu, struct task_struct *prev) {
    (void)prev;

    /* 当前 cpu 已被剔出活跃集合：不再向其 LOCAL 队列填充任何任务，
     * 让 cpu 进入 idle。已挂在它 LOCAL_ON 上的任务跑完当前 slice
     * 就会通过 enqueue 重路由（task_cpu_allowed 级联拦截）。*/
    if (!valid_cpu(cpu) || !cpu_is_active((__u32)cpu))
        return;

    /* 余下逻辑同原版：drain inactive_dsq(self) → READY_DSQ → inactive_dsq(self)
     * → steal_inactive */
}
```

`steal_inactive` 内部不需要改动：它由活跃 cpu 调起、把任务搬到自己的
LOCAL，恰好是我们想要的"残留任务从非活跃 cpu 的 inactive_dsq 流出"的
路径之一（与 syscall prog 里的批量 drain 互补）。non-active cpu 不可能
执行到 `steal_inactive`，因为已被上面的 early return 截胡。

### Syscall 程序

```c
/* intf.h */
struct accordin_active_cpus_args {
    __u8 wanted[MAX_CPUS];   /* 1 = active, 0 = inactive */
    __u32 nr_cpus;           /* 实际有效长度，<= MAX_CPUS */
};
```

```c
/* main.bpf.c */
SEC("syscall")
int accordin_set_active_cpus(struct accordin_active_cpus_args *args) {
    struct bpf_cpumask *mask;
    __u32 i, n;
    bool was_active[MAX_CPUS];

    if (!args)
        return -EINVAL;
    n = args->nr_cpus;
    if (n > MAX_CPUS)
        n = MAX_CPUS;

    bpf_rcu_read_lock();
    mask = active_cpumask;
    if (!mask) {
        bpf_rcu_read_unlock();
        return -EINVAL;
    }

    /* 1) 先记录哪些 cpu 当前活跃，用于发现 newly-inactive */
    for (i = 0; i < MAX_CPUS; i++)
        was_active[i] = bpf_cpumask_test_cpu(i,
                                             (const struct cpumask *)mask);

    /* 2) 根据 wanted 重写 mask */
    for (i = 0; i < MAX_CPUS; i++) {
        bool target = (i < n) && args->wanted[i];
        if (target)
            bpf_cpumask_set_cpu(i, mask);
        else
            bpf_cpumask_clear_cpu(i, mask);
    }
    bpf_rcu_read_unlock();

    /* 3) 排空"刚变非活跃" cpu 的 inactive_dsq → READY_DSQ。
     *    虽然 dispatch 早退已经保证非活跃 cpu 自己不会再 drain 自己的
     *    inactive_dsq，且 active cpu 的 steal_inactive 会逐步取走，但
     *    主动批量 drain 一次能保证：
     *    - 短期内残留任务确定性流出，不被 steal 路径的扫描窗口
     *      (INACTIVE_STEAL_SCAN=8) 限制
     *    - 缩容到极端少数 cpu 时不会有任务卡住没人偷
     *    kick(i) 用于让 cpu i 立刻走一次 dispatch 路径（命中 early
     *    return），从而尽快进入 idle 不再持有调度状态。*/
    for (i = 0; i < MAX_CPUS; i++) {
        bool target = (i < n) && args->wanted[i];
        if (was_active[i] && !target) {
            bpf_repeat (MAX_TASKS) {
                if (!scx_bpf_dsq_move_to_dsq(inactive_dsq_id(i),
                                             READY_DSQ_ID))
                    break;
            }
            scx_bpf_kick_cpu(i, 0);
        }
    }
    return 0;
}
```

注：`scx_bpf_dsq_move_to_dsq` 的精确 API 名称在实现期 cross-check
（scx 提供 `scx_bpf_dispatch_from_dsq` / `scx_bpf_dsq_move` 等变体；选定
"把 dsq A 队头任务移到 dsq B"的那一种，可能需要配合 `bpf_for_each(...,
SCX_DSQ_ITER_*)`）。

`bpf_rcu_read_lock` 是因为 syscall 程序是 sleepable，verifier 通常要求 kptr
读取在 RCU 读侧。set/clear 在锁内执行，外部读者观察到的两端态是一致的。

### Maps / intf 调整

`maps.bpf.h` 不新增 map。`intf.h` 仅新增 `struct accordin_active_cpus_args`
和必要的 `MAX_CPUS` 一致性（已存在）。

## User space side

### `cpu_affinity.rs` 改动

1. **删除生成式遍历线程的路径**：
   - 删除 `apply_process_configured_affinity`、`apply_process_affinity`、
     `current_process_tids`。这些只服务于动态收缩。
   - `update_dynamic_cpu_count` 中原来的
     `apply_process_configured_affinity(config)?;`
     改为：构造 `[u8; MAX_CPUS]` bitmap（前 `applied_cpus` 个
     `available_cpus` 置 1，其余置 0），调用注册的 BPF sink 推过去。
   - 推送失败：把 `active_cpu_count` 回滚到 `previous_cpus`，返回 `Err`。

2. **简化 generation/per-thread apply**：
   - 删除 `generation: AtomicU64` 和
     `CURRENT_THREAD_AFFINITY_GENERATION`。
   - `ensure_current_thread_affinity` 改成"每线程只 apply 一次初始 mask"
     （用 thread-local `Cell<bool>` 的 `applied`），保留语义：新线程进入
     measurement 时一次性把 init mask 应用到自己。
   - 这条路径只对应 `init_from_env` 时的初始 process-level 缩限，没有竞态。

3. **`init_from_env` 增量**：
   - 保留原有 `apply_current_configured_affinity(config)` 调用（当前线程
     的 `sched_setaffinity`）。
   - 在它之后**还需要**通过 BPF sink 推一次初始 mask，让 BPF 端的
     `active_cpumask` 从一开始就反映正确的 active 集合（防止 attach 后
     调度回调在 `active_cpumask` 仍为全 0 时把任务卡死或全部 fail-open）。
   - **顺序约束**：BPF sink 注册必须发生在 `scx_ops_attach!` 之前，且
     初始 mask push 完成后才能 attach；否则 attach 后到 push 之间存在
     fail-open 窗口（虽然行为 OK，但会有 latency spike）。
   - 实际生效的注册流程见下一节"Bridge"。

### Bridge: `scheduler_loader.rs`

新增模块 `bpf_active_cpus`（或就地放在 `cpu_affinity` 里），定义：

```rust
pub trait BpfActiveCpusSink: Send + Sync {
    fn push(&self, wanted: &[u8; MAX_CPUS]) -> Result<(), String>;
}
static BPF_SINK: OnceLock<Box<dyn BpfActiveCpusSink>> = OnceLock::new();
pub fn set_bpf_sink(sink: Box<dyn BpfActiveCpusSink>);
pub(crate) fn push_active_mask(wanted: &[u8; MAX_CPUS]) -> Result<(), String>;
```

`scheduler_loader.rs` 在 `scx_ops_load!` 之后、`scx_ops_attach!` 之前：

1. 从 `skel.progs.accordin_set_active_cpus` 拿到 `BorrowedFd`/raw FD（libbpf-rs
   `Program::as_fd()`）；存入 `AtomicI32` 或专门的 sink 结构体。
2. 实现 `BpfActiveCpusSink::push`：用 `libbpf_sys::bpf_prog_test_run_opts`
   或 libbpf-rs 的 `Program::test_run_opts` 调起 syscall prog，传入
   `accordin_active_cpus_args`。
3. `cpu_affinity::set_bpf_sink(...)` 注册。
4. 调一次 `cpu_affinity::push_initial_mask_to_bpf()`（新增 API）：用当前
   `active_cpu_count` 构造 bitmap 并通过 sink 推到 BPF。
5. `scx_ops_attach!`。

### `MAX_CPUS` 共享

Rust 端常量需要与 BPF `intf.h` 的 `MAX_CPUS` 同步。当前代码已经在 Rust 端
通过 `bpf_intf` bindgen 暴露 `MAX_CPUS`；沿用。

## Failure modes

| 场景 | 现行 | 改后 |
|---|---|---|
| `bpf_cpumask_create` 失败 | 不存在 | `accordin_init` 返回 `-ENOMEM`，加载失败（与现有 DSQ create 失败处理一致） |
| Syscall prog `bpf_prog_test_run` 失败 | 不存在 | `update_dynamic_cpu_count` 回滚 `active_cpu_count`，返回 `Err`，`lock_stats` 走 `log_dynamic_cpu_affinity_error` |
| BPF 还没初始化 mask 但已开始调度 | N/A | `cpu_is_active` 在 `mask == NULL` 时返回 true，等价于"全 cpu 活跃" —— 与原 `task_cpu_allowed` 一致，安全 |
| 用户态 `init_from_env` 失败、BPF sink 没注册 | 旧逻辑直接 fall-through | 同样 fall-through；BPF mask 保持空（fail-open），即"等价于不限制"，行为与禁用动态 affinity 一致 |

## Testing

1. `cpu_affinity` 单元测试：删掉与 `apply_process_affinity` 相关的部分；
   新增对 bitmap 构造逻辑的测试（前 N 个 `available_cpus` 置 1）。
2. 集成 smoke：手动跑一个 N→M→N 的 cpu 缩放序列，确认：
   - 用户态 `current_dynamic_cpu_count` 与预期一致。
   - 通过 `bpftool map / prog` 验证 syscall prog 被调起且没报错。
   - 不再观察到 sched-ext exit 因 `cpus_ptr` mismatch 报错。
3. 压测：在高 load 下反复触发 `update_dynamic_cpu_count`，BPF 调度器不应
   因 cpus_ptr 校验失败而 abort（这是当前要修的具体症状）。

## Migration / rollout

- 单 commit 改动即可（同时改 BPF 与用户态，因为接口是新的 syscall prog +
  新 sink trait）。
- 保留 `init_from_env` 的 `sched_setaffinity` 路径作为"belt-and-suspenders"，
  即使 BPF mask 因任何原因没生效，进程级 affinity 仍受限。

## Open items

1. `scx_bpf_dsq_move_to_dsq` 的确切签名：实现期对照 scx 头确定，可能要换成
   `bpf_for_each(scx_dsq, p, inactive_dsq_id(i), 0) { scx_bpf_dispatch_from_dsq(...) }`
   形式。
2. `bpf_prog_test_run_opts` 在 sched_ext 加载完之后是否可在主线程任意上下文
   调用：libbpf 实测可行；如发现限制，备选用一个固定后台线程池一并 batch
   推送。
3. 是否需要把 `init_from_env` 的初始 mask push 也走 sink：上文方案是"是"，
   原因是避免 attach 后早期 fail-open 窗口；但 fail-open 等价于"不限"，影响
   只是早期 spike，可接受。如果 review 认为可接受，可省一步初始 push。
