# 删除 owner DSQ 后的 TPS 核对

后续更新（2026-09-05）：授权更新已合入主代码，保留直接 local 路径，移除 owner DSQ。合入后的验证见 [integration.md](integration.md)。下文保留合入前的实验记录，其中“当前版本”和“尚未合入”均指当时的状态。

2026-09-05 UTC，在当前机器用同一份 mutexbench 二进制，交替加载修改前保存的动态库和修改后动态库。**没有复现删除 owner DSQ 后再下降 50%：修改前后都约 53 万 TPS，中位数差异为 +0.033%。** 当前约 53 万 TPS 相对旧分支的回退仍然存在，授权更新实验可以恢复吞吐量。

本次沿用此前的 192 线程、MCS-TAS direct、CPU 0–95、单锁、CS 300 ns / NCS 3000 ns 配置。每个无计数器版本交替运行 3 次，预热 1 秒、测量 5 秒；校准 9/32，采样 stride 8，TSE 关闭。分别显式设置动态库绝对路径，开启 BPF/admission，关闭 stats-only。修改前库来自上一轮修改前保存的 `target/local-owner-validation/before/`，其 SHA-256 与上次回归调查的 current 库一致。

## 修改前后对照

| 版本 | TPS 中位数 | 最小–最大 |
| --- | ---: | ---: |
| 修改前：owner DSQ | 532,798.59 | 522,432.90–534,021.69 |
| 修改后：直接 local，删除 owner DSQ 创建和搬运 | 532,974.11 | 532,467.76–540,881.01 |
| 早先实验：仅改为直接 local，保留空 owner DSQ | 536,455.75 | 532,112.92–550,949.93 |

这组对照分别检查了改变入队目标，以及删除空队列及搬运调用的影响；没有测到 53 万降至 26 万的情况。不能据此排除其他负载的退化；本次没有收到另一份运行命令或具体的修改前后数值。

## 当前低 TPS 的来源

当前主库仍然没有 `accordin_yield` 授权更新回调。此前约 136 万 TPS 是独立实验库的结果，该补丁没有合入 `src/` 或 `target/release/`。本次将同一个最小授权更新逻辑移植到 local 队列版本，在独立目录构建并再次交替验证：

| 版本 | TPS 中位数 | 最小–最大 |
| --- | ---: | ---: |
| 当前 local 版本 | 534,795.50 | 529,634.52–534,951.17 |
| local + 在 yield 中更新自己的授权（实验） | 1,345,378.52 | 1,340,647.34–1,361,712.23 |
| owner DSQ + 同一授权更新逻辑（原实验） | 1,382,926.48 | 1,355,763.46–1,413,388.25 |
| 旧 refactor_scheduler 分支 | 1,083,568.41 | 1,072,502.58–1,085,714.63 |

保留 local 队列时，仅加入授权更新路径，TPS 就从 534,796 恢复到 1,345,379，提高 151.57%。两个带授权更新的版本相差约 -2.72%，没有减半。

当前 local 版本附加用户态 TLS 计数器，单独测量 5 秒、不预热：

- 完成 2,795,223 次操作，进入 admission 慢路径 65,343 次。
- 累计 yield 30,175,036 次，平均每次慢路径 461.79 次，单次最多 1,535 次。
- 校验失败时读到自己的旧 ticket 30,070,638 次，占失败检查的 **99.870%**；读到空 owner 39,055 次，其他线程 owner 为 0 次。

具体链路仍是：外层加锁递增 generation → 用户态要求本次 ticket 与 CPU owner 完全一致 → BPF 还保留该线程上一轮 ticket → 当前线程反复 yield，等待旧名额被回收再重新排队获准。解锁仅清除用户态状态标志，不立即清除 CPU owner。改变 owner 入队目标并没有补上授权更新路径。

在 Linux v6.14 中，默认 yield 会耗尽 slice；但没有其他可运行任务时，`balance_one()` / `pick_task_scx()` 可以继续运行 prev，因而 yield 返回并不保证执行 stopping/enqueue。具体实现见 [Linux v6.14 sched_ext](https://github.com/torvalds/linux/blob/v6.14/kernel/sched/ext.c)。先前实验还表明，单独减少重复 yield 并不能恢复吞吐量，重新排队及锁交接行为也参与了回退，详见 [此前定位报告](../refactor-scheduler-regression/README.md)。

## 结果范围与复现

本次没有修改项目核心源码，保留上一轮 local 队列简化。授权更新和计数器都仅用于独立诊断。授权续用会改变等待队列的服务顺序，尚不能把这份原型的吞吐量当作经过公平性验证的正式修复。

共 22 次 benchmark 全部成功；每次校验 BPF 加载日志、enable_seq、运行期间 enabled 状态，校验输出包含 192 个非零线程计数且求和与总操作数一致；最后 sched_ext 状态为 disabled。修改前、修改后主库哈希和测试命令保存在 [metadata.json](metadata.json) 与 [results.csv](results.csv)，完整日志见 [runs.jsonl](runs.jsonl)。计数器汇总见 [counters.json](counters.json)，诊断差异见 [renew-local-experiment.patch](renew-local-experiment.patch)、[user-counters.patch](user-counters.patch)。

同一工作区可复测，输出标签须尚不存在：

```sh
python3 target/local-owner-regression/run.py reproduce-rerun \
  before after local_only --repeats 3 --duration 5000 --warmup 1000
python3 target/local-owner-regression/run.py confirmation-rerun \
  after after_renew renew_clean refactor --repeats 3 --duration 5000 --warmup 1000
python3 target/local-owner-regression/run.py counters-rerun \
  after_counters --repeats 1 --duration 5000 --warmup 0
```
