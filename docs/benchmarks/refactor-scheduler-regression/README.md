# refactor_scheduler → simplify：192 线程 TPS 回退定位

后续更新（2026-09-05）：授权更新已合入主代码，使用直接 local 路径，移除 owner DSQ。合入后的验证见 [integration.md](../local-owner-regression/integration.md)。下文保留合入前的实验记录，其中“当前版本”和“尚未合入”均指当时的状态。

2026-09-05 UTC，在相同机器和 mutexbench 二进制上重测，确认当前实现相对 `refactor_scheduler` 下降约 50%。回退来自简化时改变的 admission 请求生命周期：每次外层加锁递增 ticket，但 BPF 没有为仍占有当前 CPU 名额的线程更新授权的路径。新请求只能等待旧名额被回收，再经全局等待队列获得授权。

这同时造成旧 ticket 上的反复 yield，以及锁等待队列需要重新建立的问题。**只消除重复 yield 不能恢复 TPS；在 BPF 中确认当前线程仍拥有该 CPU 名额并更新 ticket，保留用户态严格校验，可以恢复吞吐量。** C 迁移前的简化 Rust 版已经出现回退。

本次只在独立实验目录修改实现。可审阅的最小实验补丁为 [renew-own-slot.patch](renew-own-slot.patch)，尚未应用到项目源码。

版本与第一组交替测试结果如下；每种实现 3 次，预热 1 秒、测量 3 秒，表中为中位数：

| 实现 | 提交 | TPS（ops/s） | 最小–最大 |
| --- | --- | ---: | ---: |
| refactor_scheduler | `d038f30` | 1,056,154 | 1,055,751–1,059,288 |
| 简化后的 Rust | `695c873` | 491,874 | 489,823–507,879 |
| 当前 C | `f9fd780` | 532,286 | 529,611–533,230 |

当前 C 比旧分支低 **49.60%**。之前的 [C 迁移报告](../c-migration-192/README.md) 比较的是 `695c873` 与 C 实现，不能据此排除相对 `refactor_scheduler` 的退化。

定位后，对没有诊断计数器的最小补丁重新交替测量，各 3 次、预热 1 秒、测量 5 秒：

| 实现 | TPS 中位数（ops/s） | 最小–最大 |
| --- | ---: | ---: |
| refactor_scheduler | 1,077,686 | 1,065,560–1,091,325 |
| 当前 C | 535,972 | 530,928–545,203 |
| 当前 C + 在 yield 中更新自己的授权 | 1,360,533 | 1,347,995–1,377,374 |

补丁版本相对当前实现 **+153.84%**，相对旧分支 **+26.25%**。这证明缺失的授权更新路径足以解释本负载的退化；不代表它已满足所有负载的公平性要求。

当前协议的具体执行顺序：

1. [admission_begin](../../../src/runtime.h#L30) 每次外层加锁递增请求编号；解锁只清除用户态标志，不立即更新 BPF owner。
2. [admission_wait](../../../src/runtime.h#L41) 发布 WAITING，循环调用 `sched_yield()`，直到 `owners[current_cpu]` 等于本次请求的 ticket。
3. [accordin_dispatch](../../../src/bpf/main.bpf.c#L126) 忽略 `prev`；[admit_waiter](../../../src/bpf/main.bpf.c#L102) 看到 CPU owner 非零就返回，即便这个 owner 是当前线程的上一次请求。
4. 如果没有任务可供切换，sched_ext 可以继续运行当前线程。因而 yield 返回后，用户态再次读到自己的旧 ticket，继续 yield。旧请求通常要等 tick 或实际切换时的 stopping/enqueue 才被处理。
5. 即使提前回收旧名额，新请求仍需重新入队、获准并进入原始锁队列。这个过程也改变了连续锁交接的行为。

第 4 点的内核语义可在 Linux v6.14 的 [`yield_task_scx`、`balance_one` 和 `pick_task_scx`](https://github.com/torvalds/linux/blob/v6.14/kernel/sched/ext.c) 中核对：默认 yield 耗尽 slice，但没有可运行的替代任务时允许继续运行 `prev`。本机运行内核为 Ubuntu `6.14.0-37-generic`；还用只读反汇编核对了实际 `yield_task_scx` 的默认 slice 清零路径。不能将 `sched_yield()` 返回等同于一次完整的停止、重新入队和授权。

旧分支 `src/accordin_shared/src/mutex_hook.rs` 的 `lock_scope_with_stats` 在慢路径上只 yield 一次，随后直接进入原始锁；没有等待本次请求 ticket 的确认。它还通过 `TOKEN_CONSUMED` 影响下一次加锁的快慢路径。因此，这次简化改变了准入行为，并非只是删除 width/CV 代码或替换加载框架。

计数器给出的证据：

| 5 秒运行，未预热 | 完成操作数 | sched_yield 次数 | yield / 操作 |
| --- | ---: | ---: | ---: |
| refactor_scheduler | 5,277,775 | 5,277,751 | 1.00 |
| 当前 C | 2,678,328 | 26,879,796 | 10.04 |

独立的用户态 TLS 计数器运行完成 2,774,330 次操作，其中 64,101 次进入 admission 慢路径（2.31%），累计 yield 24,059,661 次。每次慢路径平均 yield **375.34 次**，最大 1,242 次。失败检查中 23,959,158 次读到当前线程的旧 ticket，36,402 次读到空 owner，读到其他线程 owner 的次数为零；旧 ticket 占失败检查的 **99.85%**。

BPF 计数器进一步记录到约 2,126 万次 dispatch 同时满足“prev 仍占有 CPU 名额、用户态请求编号已改变”，相应的 `admit_waiter` 被非零 owner 拦下。该轮 `bpf_probe_read_user` 失败次数为零。这把问题定位到了请求生命周期，而不是用户态地址读取失败。

为避免把相关性当作原因，进行了以下单独修改实验：

| 实验 | TPS（ops/s） | 结论 |
| --- | ---: | --- |
| 仅把 ARM `isb` 改回旧分支的 `yield` | 541,845 | 同组当前值 542,278，无恢复 |
| 将 owner 直接放入 local DSQ | 540,469 | 无恢复 |
| enqueue 时尝试立即授权 | 535,427 | 无恢复 |
| 仅在 NORMAL 队列非空时调用搬运 helper | 547,870 | 同组当前值 546,976，无恢复 |
| 在 dispatch 中刷新 prev 的旧请求 | 516,097 | 无恢复 |
| 设置 `SCX_OPS_ENQ_LAST` | 526,230 | 无恢复 |
| 跳过确认，只 yield 一次 | 1,048,795 | 恢复，但不再确认实际授权，仅用于诊断 |
| 在 yield 中回收旧名额，仍重新排队 | 535,862 | yield 大幅减少，TPS 未恢复 |
| 在 yield 中更新仍属于自己的名额 | 1,360,533 | 保留 ticket 校验，恢复 TPS |

前七项是每项 3 次的中位数；“yield 中回收”是带诊断计数器的单次 5 秒运行；最后一项为上述无计数器确认组。各项应与其所属实验组的当前实现比较，完整记录见 [results.csv](results.csv)。其他组合实验也保留在数据中。

“yield 中回收”将单次运行的 yield 从 2,132 万次减少到 65,635 次，仍只有约 53.6 万 TPS。这排除了“只要减少 syscall 次数就能解决”的解释。额外强制所有外层加锁走慢路径，继续使用原有 ticket 重排队协议时只有 13,639 TPS；同时允许更新自己的名额则为 1,356,306 TPS。该实验说明当前约 53 万 TPS 还依赖大量快路径成功，不能单纯删掉快路径来恢复旧行为。

实验补丁只增加一个 BPF yield 回调和对应注册。仅当线程状态为 WAITING、已有名额属于当前 CPU 时，才用 CAS 将 owner 的旧 ticket 替换为本次 ticket；成功后同步更新 task context。它没有把用户态校验改成只比较 TID，也没有把另一个线程的名额交给当前线程。yield 仍耗尽 slice，原有回收、队列和迁移路径继续执行。

这一方向复用了现有 direct API smoke：两个后端均通过无 BPF（允许 96 CPU）、单 CPU BPF、双 CPU BPF 并改变 affinity 的检查，共 6 项。另行尝试的无 BPF、8 线程限制在 2 CPU 的 MCS 检查在 30 秒后超时，未计为通过，也未进一步定位该额外配置。完整记录见 [smoke.json](smoke.json)。

补丁仍是诊断原型：允许连续请求更新自己的名额会改变等待队列的服务顺序，不能由短时单锁 benchmark 和 smoke 推断任意竞争模式下的公平性。集成时需要确定何时续用、何时把名额交给已排队线程。

测量条件与复现信息：

- HiSilicon TaiShan-v110，96 个物理核，无 SMT，双路、4 NUMA 节点；192 线程，affinity `0-95`。
- 同一个 MCS-TAS direct 单锁 benchmark，CS 300 ns、NCS 3000 ns，burn 校准为编译默认 `9/32`，timing sample stride 8，timeslice extension 关闭。
- mutexbench 源码来自当前子模块 `60f569e6e100d3f415d2aac2ce97edc74f9acf23`，编译命令为 `g++ -O3 -std=c++20 -pthread bench/mutexbench/mutex_bench.cpp -o target/regression-refactor/bin/mutex_bench -ldl`。二进制 SHA-256 为 `b3117c38f6f1a6782e4831fb429e7e429fa70e104e696616bc63bcf870ba54aa`。没有变更用户的子模块指针。
- 旧分支在独立 worktree 中从 `d038f301965f658fa5cba13f925ba987ac069c64` 以 `cargo build --locked --offline --release` 重建。简化 Rust 使用上次迁移前保存的库。各库哈希见 [metadata.json](metadata.json)。
- BPF/admission 开启、stats-only 关闭。一次只运行一个调度器，检查加载日志、enable_seq、运行期间的 enabled 状态和结束后的退出状态；85 次性能运行均成功完成。
- 性能确认组每次预热 1 秒、测量 5 秒，各实现 3 次，交替顺序。perf/计数器实验测量 5 秒、预热 0 秒；perf 包含启动和退出阶段，不应把其 CPU 时间与只含测量阶段的 TPS 直接换算成每操作耗时。
- 本机不是隔离的专用测试环境；结论限于该机器、192 线程和该单锁负载。没有据此推断其他 CPU、线程数、MCS 后端或多锁负载的性能。

原始日志合并存于 [runs.jsonl](runs.jsonl)，每条含实验组、case、轮次、原日志文件名及完整输出；[results.csv](results.csv) 保留指标和执行命令，log 列对应 JSONL 中的 `log_file`。[perf-counters.csv](perf-counters.csv) 保存原始 perf 计数；[diagnostic-patches.txt](diagnostic-patches.txt) 按 `CASE:` 分隔保存诊断差异。二进制、独立源码和 runner 保存在工作区的 `target/regression-refactor/`。

在本工作区复测确认组（输出标签需尚不存在）：

```sh
python3 target/regression-refactor/run.py confirmation-rerun \
  current renew_clean refactor --repeats 3 --duration 5000 --warmup 1000
```

最小补丁可在 `f9fd780` 的独立 checkout 应用后用现有 Makefile 构建。不要把 `one_yield_unchecked` 诊断变体作为修复；它通过省略准入确认获得吞吐量，不能证明每 CPU 单等待者约束。
