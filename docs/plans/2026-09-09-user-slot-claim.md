# 用户态名额认领

状态：已实现。调度器侧见 `e25ab5f`，用户态快路径见 `ef669b7`，分支 `cv_admission`。本文记录规则、依据和验证方式，不是待批准的方案。

## 问题

慢路径每次都进内核。外层 `raw_trylock` 失败后，`admission_wait` 发布 `USER_WAITING` 再 `sched_yield()`：`ops.yield` 可能续用本 CPU 的旧名额，否则 `ops.enqueue` 把线程放进 bank（`WAITING_DSQ + cpu`），等 `ops.dispatch` 授权，用户态再读 `admission_state.owners[cpu]` 确认。哪怕本 CPU 的名额空着、全机没有一个等待者，这一趟 syscall 加重调度也照付，而临界区常常只有几十纳秒。

名额表本来就是 libbpf 给进程的可写共享映射，用户态一直在读 `owners[]`。既然如此，没有竞争者时线程可以自己把 ticket 写进去。要让这件事成立，必须同时回答两个问题：新请求不能越过 bank 里更老的等待者，以及调度器从未见过的名额由谁回收。

## 规则

`owners[cpu]` 仍是唯一的名额记录，值仍是 `request << 32 | tid`。写它的四方都只在自己的 ticket 值上操作：dispatch 的 `admit_from` 做 `0 → ticket`，调度器的 `release_slot`、`exit_task` 扫描和 `ops.exit` 清表做 `ticket → 0`，`ops.yield` 做 `old → new`，用户态做续用、认领和撤销。内核侧一律用 CAS；用户态在本 CPU 的表项上改用 rseq，见「用 rseq 写名额」。

慢路径在策略启用时按顺序尝试三件事：

1. **确认已有授权**。condvar 唤醒，或被 flush 送回 bank 后由 dispatch 授权的请求，名额已经写好，读到 `owners[getcpu()] == ticket` 就直接返回，并把 `slot`/`ticket` 记进 `thread_state`，让调度器的记录和这张表指向同一项。
2. **续用或认领**，条件是 `ACCORDIN_USER_CLAIM` 开启且 `demand <= 0`。续用是把 `owners[slot - 1]` 从自己上次写的 ticket 换成本次请求；失败说明名额已被回收或已属他人，清掉记录后退到认领。认领是读当前 CPU，该项为 0 时先写 `thread_state.slot`，再把它从 0 换成本次 ticket。交换用 CAS 时，写成功后还要重读 CPU，若已不在该项所属的 CPU 上则 CAS `ticket → 0` 撤销；不撤销的话，名额留在一个没人跑的 CPU 上，而线程实际所在的 CPU 上有两个自旋者。撤销总是安全的：这个值只有自己写，调度器若已收养，它稍后的 release CAS 会失败并只清自己的记录。这两次交换在有 rseq 的机器上不用原子指令，见下节。
3. **排队**。以上都不成立就发布 `USER_WAITING`，进原有的 yield 确认循环，确认后同样记录 `slot`/`ticket`。

`sched_getcpu()` 是 vDSO/rseq 读，不是 syscall。名额粘性，`admission_finish` 不释放。

### 为什么先发布 SPINNING

第一条语句是把 word 写成 `request | USER_SPINNING`，在看表之前。三种取值只有它可用：

- `WAITING`：操作做到一半被抢占，enqueue 会把线程放进 bank，dispatch 可能在别的 CPU 授它名额，恢复后自己的 CAS 又拿一个。
- 空闲（flags 为 0）：认领后被抢占，`refresh_episode` 会当作 episode 结束把名额收掉。
- `SPINNING` 且尚无名额，在调度器眼里就是"已在 raw 队列的线程"：enqueue 的两个分支都要求 `WAITING`，所以进 `NORMAL_DSQ`；stopping 有显式的 SPINNING 守卫；tick 与 select_cpu 都以 `admission_cpu` 为门；runnable/quiescent 不看 word。认领成功后再被抢占走收养路径，回到 `SCX_DSQ_LOCAL_ON`。

## demand

`admission_state.demand` 是有符号的 advisory 计数，表示 `NORMAL_DSQ` 与整个 bank 里排队的任务数。enqueue 放进这两处、custody 过期移入 `NORMAL_DSQ`、flush 移入 bank 时加一；`scx_bpf_dsq_move_to_local(NORMAL_DSQ)` 成功和 `admit_from` 的 move 成功时减一。加减只在 `lock_routing()` 为真时进行，路由关闭时这个计数没有读者。`ADMIT_SLOT_LOST` 和 move 失败不减。加减都是无条件原子操作，不做饱和判断。

`cv_scan` 定时器（1–10 ms）在函数最前面无条件校正：先快照 `demand`，再数 `nr_queued(NORMAL_DSQ) + nr_waiting()`，把差值原子加回去。用差分而不是覆盖，是因为覆盖会抹掉走表期间发生的入队，让非空的 bank 读成 0。`ops.exit` 清 0。

漂移只在一个扫描周期内存在：偏大只多 yield 几次，偏小最多让一个周期内的请求越过队列。任务不经上述路径离开 DSQ 的情况（affinity 变更触发的 dequeue 与再 enqueue、排队中退出、卸载排空）都由校正吸收，因此不实现 `ops.dequeue`。

把 `NORMAL_DSQ` 一并计入是必需的：每次 yield 都会触发本 CPU 的 `ops.dispatch`，顺手把普通任务搬到本地队列；不 yield 的线程只在 tick 或时间片用完时才让 dispatch 跑，普通任务的等待会从微秒级变成 tick 级。只要有普通任务排队就照旧 yield，这条路径没有变化。

## 收养

调度器所有决定（select_cpu 钉住、enqueue 放 `SCX_DSQ_LOCAL_ON`、tick 为普通任务让出切片、stopping 与 refresh 回收）都以 task storage 里的 `admission_cpu`/`ticket` 为据，所以用户态写下的名额必须被认下来。

`intf.h` 定义 `struct admission_word { u32 state; u32 slot; }`，`thread_state` 以它开头并 `_Alignas(8)`，两侧的偏移与对齐由 `_Static_assert` 锁定，`user_state` 一次读 8 字节。对齐是硬要求：4 字节对齐的一对可能落在页尾，8 字节读跨到未映射页就永远失败，那个线程从此既不入 bank 也不被回收。两个 32 位字段分开写，不合成一个 64 位原子，因为 `relock_wake` 由别的线程写低 32 位。

收养放在 `refresh_episode` 最前面（enqueue、tick、stopping 共用）：

- `slot` 非 0 且 `slot - 1 < waiting_queues`（init 记下的队列数，即 `min(nr_cpu_ids, MAX_CPUS)`）。这是用户可写的值，越界的 CPU 传给 `scx_bpf_kick_cpu` 会让整个调度器退出。
- 把 `owners[slot - 1]` 读一次到局部变量，要求低 32 位等于 `p->pid`，且它的请求号不大于 word 里的请求号。后一条挡住撕裂读：word 与 slot 是两次独立的 4 字节写，内核可能读到旧 word 配新 slot，此时按"word 空闲即回收"的规则会把刚认领的名额收掉；请求号单调递增，旧 word 认不出新 ticket，于是跳过这次收养。
- 记录指向另一个 CPU 时，先对旧记录 `release_slot`。用户态续用后旧 ticket 仍在旧表项里，这个 CAS 常常成功，它是唯一释放旧表项的地方。
- 然后写 `admission_cpu` 与 `ticket`，只有真正变化才计数，稳定状态不会每个 tick 都记一笔。

收养必须在 affinity 检查之前，`DIRECT_SMOKE_MIGRATE` 依赖这个顺序。tid 匹配保证不会收养别人的名额。`release_slot` 改为只在 CAS 成功时 `kick`：收养来自用户可写的 `slot`，否则每次不匹配都会向无关 CPU 发一次 IPI。

## 回收的兜底

- **调度器没见过的名额**：tick（HZ=1000）或 stopping 收养后按现有规则回收；解锁后进 futex 睡眠的线程在 stopping 就回收，不必等 tick。
- **线程退出**：`exit_task` 时 mm 可能已释放，读不到 word，所以无条件按 `waiting_queues` 扫一遍 `owners[]`，低 32 位等于 `p->pid` 的项 CAS 成 0，只对成功的项 kick。扫描覆盖了记录里那一项，`exit_task` 不再单独 release。这笔开销只在线程退出时付，被信号杀死也不漏。
- **fork**：子进程共享同一块 bss 映射，`pthread_atfork` 的子进程钩子清掉继承来的注册状态，让它下次加锁以自己的 tid 重新注册；否则子进程会用父线程的 tid 写表。
- **卸载**：`ops.exit` 用同一个扫描（不带 tid，即扫全表）数一遍并清空 `owners[]`，非零项数写进 `slots_left`。不能在析构函数里数：那时活着的线程本来就合法持有粘性名额。

## 竞争分析

每条都依赖"CAS 只动自己 ticket"这一性质：

- 认领对同 CPU 的 dispatch 授权：只有一个成功。dispatch 输了走 `ADMIT_SLOT_LOST`，等待者留在队里；用户态输了走 yield。
- 续用对内核回收（tick 落在新请求发布之后、CAS 之前）：内核可能先做 `old → 0`，续用失败，退到认领或 yield。丢一次续用，没有正确性问题。
- `ops.yield` 续用写入的 ticket 用户态没见过：yield 循环确认的正是本请求的 ticket，确认时就记录下来，不会过期。
- 永久双名额不可达：用户态只写 `owners[slot - 1]`，且只从自己写过或确认过的值 CAS；内核只授权 bank 里的任务，而 bank 里的任务不在运行；进 bank 要求 `admission_cpu == 0`，收养在此之前发生。

## 用 rseq 写名额

续用和认领各自只有一次比较加一次写入，而且写的是本 CPU 的表项，因此不需要原子指令，只需要"没跑完就不算数"。这两步放进一段 restartable sequence：普通 load、比较、单条 store，内核在抢占、迁移和信号投递时把线程从 abort handler 重新拉起来，commit 的 store 永远不会在线程离开这个 CPU 之后才执行。

为什么这样就够：线程在 CPU X 上跑的时候，唯一能把 `owners[X]` 从空变成授权的写者是 X 自己的 `ops.dispatch`——`admit_from` 只授权正在 dispatch 的那个 CPU 的名额——而 X 上要跑 dispatch，必须先把这个线程调度下去，那会重启这段。其余写者只 CAS 自己的 ticket：别的任务的 `release_slot`、`exit_task` 的扫描、`ops.exit` 的清表，认的都是这个线程从没写过的值，碰不到 0，也碰不到本线程持有的项。X 上的 tick 可能不重新调度就把本线程的旧 ticket 清成 0：此时 store 落在一个空项上，正是续用要的结果，调度器随后按 `thread_state.slot` 重新收养。

由此，rseq 路径上认领成功后不再重读 CPU，也没有撤销那一步：迁移在 commit 之前就把这段重启了。CAS 路径两步都保留。线程迁移之后 `slot` 还指着旧 CPU 的情况仍走 CAS：那个 CPU 一直在 dispatch，本段的 CPU 检查管不到它，只有 CAS 能保证不覆盖别人的授权。

描述符按 `struct rseq_cs` 写在 32 字节对齐的 `__rseq_cs` 段里，abort handler 放在 `__rseq_failure` 段，入口前紧挨着签名字（x86_64 `0x53053053`，aarch64 `0xd428bc00`）。这是 PIC 动态库，段里的 `.quad` 会变成动态重定位，librseq 同样如此。每次尝试结束都把 `rseq_cs` 清零，因为这个库可能在线程还活着时被 `dlclose`，字段不能继续指向已卸载的段。

被重启最多再试 3 次，之后退回排队：孤立的一次重启值得重来，一台不停打断的机器不如直接进队列。重启次数计入 `aborts`。

没有 rseq 区域（`__rseq_size == 0`）、不是 x86_64/aarch64、或 `ACCORDIN_USER_RSEQ=0` 时，两次交换都回到原来的 CAS，语义不变。

## 开关与计数

`ACCORDIN_USER_CLAIM` 默认开启，设为 `0` 时跳过续用与认领，慢路径回到"发布 WAITING 并 yield"，供同场 A/B。

`ACCORDIN_USER_RSEQ` 默认开启，设为 `0` 时续用与认领改回 CAS，慢路径的其余部分不变，供同场 A/B。`ACCORDIN_CV_COUNTERS=1` 时加载后打印一行 `[accordin_rseq] area=yes/no`，说明这一趟是否真的走 rseq。

`ACCORDIN_CV_COUNTERS=1` 时才累加用户态计数，卸载时打印：

```text
[accordin_claim] renews= claims= undone= aborts= queued= adopted= swept= slots_left=
```

| 计数 | 来源 | 含义 |
| --- | --- | --- |
| `renews` | 用户态 | 把自己持有的名额续用到本次请求 |
| `claims` | 用户态 | 认领本 CPU 的空闲名额 |
| `undone` | 用户态 | 交换后发现已迁移，撤回写下的 ticket；只出现在 CAS 路径 |
| `aborts` | 用户态 | rseq 段被重启 |
| `queued` | 用户态 | 退回 yield 循环 |
| `adopted` | BPF | 调度器收养用户态写下的名额 |
| `swept` | BPF | `exit_task` 扫表清除的名额 |
| `slots_left` | BPF | 卸载时表中剩余项 |

BPF 的三项同时进 dump，`demand` 也在 dump 里；它在 `ops.exit` 清 0，运行时读到的只会是 0，所以不进这一行。

## 验证

`make check`（无 BPF 路径，覆盖空映射时的取值）、`sudo make check-bpf`、`sudo make check-auto-bpf`、`sudo make check-claim-bpf`，以及 `make litl && make check-litl && sudo make check-litl-bpf`。`sudo env DIRECT_SMOKE_MIGRATE=1 bash scripts/test_direct_api.sh --bpf` 覆盖等待期间改 affinity。

`make check-claim-bpf`（`scripts/test_user_claim.sh` 与 `scripts/tests/user_claim.c`）把进程绑在两个 CPU 上，对两个后端各跑四个场景，从 stderr 按名字解析 `[accordin_claim]` 并断言：

| 场景 | 负载 | 断言 |
| --- | --- | --- |
| 低竞争 | 2 线程，临界区短，episode 之间有停顿 | `claims + renews > 0`；`queued < (claims + renews) / 2`；`slots_left == 0`；`swept > 0` 或 `adopted > 0` |
| 低竞争，`ACCORDIN_USER_RSEQ=0` | 同上 | `claims + renews > 0`；`slots_left == 0` |
| 过载 | 4 线程持续竞争 | `queued > 0`；`slots_left == 0` |
| 过载，`ACCORDIN_USER_CLAIM=0` | 同上 | `claims == renews == undone == 0`；`queued > 0` |

名额粘性，线程退出时它最后取到的那一项仍在表中，所以低竞争场景要求 `swept` 与 `adopted` 至少有一个非零：tick 可能在线程死掉之前就收养并释放了那一项，此时 `exit_task` 的扫描无事可做，`swept` 为 0。表在卸载时必须是空的，这一条不放宽。

关闭 rseq 的低竞争场景把认领和续用交给交换路径，覆盖写入后重读 CPU 与迁移撤销；这一场景不断言 `undone`，因为迁移不一定发生。

同 CPU 的第一次竞争必然排队一次，`demand` 的短暂抖动还会再添几次，所以低竞争场景要求快路径承担绝大部分，而不是全部。

## 测量

同场开关对比与吞吐回归见 [用户态名额认领测量](../benchmarks/user-slot-claim-20260909/README.md)。
