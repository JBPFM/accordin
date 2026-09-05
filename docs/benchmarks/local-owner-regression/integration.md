# local 队列与授权更新合入验证

2026-09-05，主代码已合入在 yield 中更新当前线程授权的逻辑，保留 `SCX_DSQ_LOCAL_ON | cpu` 路径，删除 owner DSQ 的定义、创建和搬运。每 CPU 的 `admission.owners[cpu]` 记录继续用于限制新等待者准入。

当线程发布 WAITING 并 yield 时，只有 task context 中的名额属于当前 CPU，且 owner 的旧 ticket 与该线程记录一致，才用 CAS 更新为本次请求的 ticket；成功后同步 task context。用户态仍需确认完整的新 ticket。yield 仍耗尽 slice，原有回收、迁移及退出路径继续执行。

两个后端均通过构建和 ABI 检查，以及无 BPF、多核 BPF、单核 BPF、双核 BPF 并改变 affinity 的现有 smoke，共 8 项运行检查。检查包含并发计数、嵌套锁、持锁时 yield/睡眠及恢复。完整输出见 [integration-checks.json](integration-checks.json)。

使用主代码重新构建的 `target/release/libmcs_tas_accordin_direct.so` 与合入前保存的 local 版本交替测试。192 线程、CPU 0–95、单锁、CS 300 ns / NCS 3000 ns；每个版本 3 次，预热 1 秒、测量 5 秒，校准 9/32，采样 stride 8，TSE 关闭。

| 实现 | TPS 中位数 | 最小–最大 |
| --- | ---: | ---: |
| 合入前 local，无授权更新 | 533,069.64 | 517,103.93–552,665.32 |
| 合入后 local + 授权更新 | 1,345,523.86 | 1,345,084.13–1,376,062.26 |

TPS 中位数提高 152.41%。6 次运行均成功，BPF/admission 开启且 stats-only 关闭；每次检查加载日志、enable_seq 与运行期间的 enabled 状态。每次均包含 192 个非零线程操作数，求和与总操作数一致。测试结束后 sched_ext 为 disabled。

等待队列按 FIFO 扫描，但已有名额可以跨请求续用，仍不保证严格 FIFO 或有界等待。本次短时 benchmark 的非零线程计数不构成无饥饿证明。

完整数据见 [integration-results.csv](integration-results.csv)、[integration-runs.jsonl](integration-runs.jsonl)、[integration-metadata.json](integration-metadata.json)。最后一份文件包含动态库绝对路径及 SHA-256。复测命令（输出标签须尚不存在）：

```sh
python3 target/local-owner-regression/run.py integrated-performance-rerun \
  after integrated_local --repeats 3 --duration 5000 --warmup 1000
```
