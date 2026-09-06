# 条件变量重获锁：预发布 admission 请求与 mutex 接力队列

实现方案 1 和 3，两个标准 LiTL 适配器共用实现。不加入方案 2 的按锁 admission 配额、锁标识或独立 DSQ；保留现有全局 WAITING_DSQ、NORMAL_DSQ 和每 CPU ticket。没有 shadow mutex，也没有新增条件变量环境开关。

## 请求与队列

```
cond queue / private futex
    │ signal: 选一个；broadcast: 选全部当前 waiter
    ▼
mutex parking queue（已通知，但仍可休眠）
    │ 首个接力者立即 wake；其余由接力者 unlock 逐个 wake
    │ wake 前 release-store 同一 epoch | USER_WAITING
    ▼
现有 BPF enqueue → WAITING_DSQ → CPU ticket / local DSQ
    ▼
relock（检查现有 grant，沿用 epoch）→ raw lock → HELD
    ▼
unlock → 下一个停车队列 waiter
```

- `include/accordin_relock.h` 与两个 direct 头文件定义 `mutex_relock_prepare`、`mutex_relock_wake`、`mutex_relock`。prepare 在旧 mutex unlock 完成后生成新 epoch，保持 flags 为零；不增加锁深度、不预占 admission 槽。
- `src/direct.c` 的 relock 只恢复深度，不调用 admission_begin。超时或取消尚未获通知的请求也由这个入口消费。`src/runtime.h` 的等待逻辑先检查预先授予的 ticket，仅在没有匹配授权时才 yield。普通竞争加锁保留原来的流程。
- `third_party/litl/src/accordin-cond.c` 保留每 waiter 独立 futex、条件变量队列及 active 计数。逻辑通知从 cond queue 摘除节点，再调用 mutex 停泊接口；没有把 futex 内核队列直接 requeue 到一个不存在的 raw-mutex futex 上。
- `third_party/litl/src/accordin.c` 保存待重获锁的 FIFO、已选择节点或已持锁的接力者身份。第一次通知即启动接力，因此 mutex 空闲、通知者未持有 mutex 时也不依赖未来的 unlock。接力者获得 raw lock 后，将栈上 waiter 引用替换为 pthread 身份；只有它的 unlock 才交出接力资格。普通竞争者仍可走原有 raw lock 路径，不保证它们和 cond waiter 之间的全局 FIFO。
- 仍持有其它 mutex 的等待者保留外层 admission episode，并绕过停泊接力立即唤醒。这里的接力只限制普通 cond waiter 变为 runnable 的时机，不限制普通竞争者，也不是 BPF 按锁 K 配额。

## 并发边界

1. **早通知与解锁：** 先登记 cond waiter，再 unlock，最后在 parking guard 下 arm。提前到达的通知先排队，直到 arm 后才能写 TLS 请求；不会覆盖旧的 USER_HELD，也不会被旧 unlock 清掉新请求。
2. **睡眠不占槽：** 外层等待的 dormant flags 为零；现有 BPF stopping/refresh 路径释放旧槽。唤醒后 USER_WAITING 才成为 admission 请求。保留其它锁的嵌套等待遵循原外层 episode 规则。
3. **通知后超时：** timedwait 到期时在 cond guard 下判定通知是否已经发生。已通知者去掉条件变量 deadline，继续等待 mutex 接力；未通知者从 cond queue 摘除后以 ETIMEDOUT 重获锁。避免到期时间造成 futex 忙循环或吞掉 broadcast。
4. **取消：** 取消在两种队列中摘除自己的节点；取消已选择者时启动下一个，signal 必要时补给另一个 cond waiter。以同一请求重获用户 mutex，再执行调用者的 cleanup。重获锁和队列操作期间禁用取消。
5. **生命周期与死锁：** 固定顺序为 cond guard → parking guard，反向不存在；不持 guard 等待 raw lock。parking guard 保持到 FUTEX_WAKE 返回，waiter 返回/销毁栈前必须经过同一 guard。未获唤醒的节点不进入 raw MCS 队列，已进入 raw 队列的节点不再停泊。

普通 lock/trylock 的路径不变；LiTL unlock 增加一次 parking 状态的 acquire load，有接力状态时才获取 guard。mutex 内部对象现在包含队列元数据，不能继续声称它只有一个指针。接力减少同时竞争，仍为每个实际唤醒执行一次 FUTEX_WAKE_PRIVATE；这次优化没有减少 broadcast 最终所需的 wake 调用总数。长临界区、混合普通竞争者的公平性和跨负载收益需要分别测量。

## 验证

- `make check`、`sudo make check-bpf`：两个 direct ABI；新用例验证其它线程发布请求、HELD/结束时 epoch 不变、未通知请求消费、嵌套请求保留外层 epoch。
- `make check-litl`、`sudo make check-litl-bpf`：两个标准 LiTL 后端，原有 mutex/condvar/C++ 测试及 NDEBUG 无原生 shadow mutex 调用测试。
- 新用例在持锁时广播两个共享 mutex 的 condvar，取消首个接力者和一个停泊者，保持 mutex 超过全部 timedwait 截止时间，再验证剩余 waiter 全部以成功返回。另测 mutex 外广播及持有额外 mutex 时的对应路径。
- 额外限制 CPU 0–1，设置 `LITL_TEST_THREADS=24 LITL_TEST_ITERATIONS=1000 LITL_TEST_TIMEOUT=180`，两个后端完整 `check-litl-bpf` 通过，包含通知后 `cond_destroy` 仍返回 EBUSY 的检查。所有测试结束后 sched_ext 恢复 disabled。

## 测量配置

使用与既有 FlexGuard suite 完全相同的 Streamcluster 和 PARSEC barrier 二进制，限制 CPU 0–95，192 个工作线程；BPF、Accordin admission 均开启，stats-only 关闭。FlexGuard 使用此前 HYBRID_MCS + BPF + CONDVARS_BLOCK 构建，其 BPF 是追踪程序，并非 sched_ext 调度器。没有使用此前诊断用的 bulk_cond shim。

```
streamcluster 10 30 512 32768 32768 2000 none <output> 192
barrier 192 100
```

Streamcluster ROI 用原始计数器 ticks / 100,000,000 转成秒；barrier 计时包含线程创建和回收。各后端串行运行，使用 `/tmp/mutexbench-sweep-multi-lock.lock` 防止测试重叠。barrier 新旧版本及 FlexGuard 各三次；完整 Streamcluster 新版本各三次，既有完整基线取此前同配置的三次结果。每次检查实际库映射、BPF fd、退出状态，以及 Accordin sched_ext enable_seq 增加一次。旧库在改动前保存到 `target/cond-relock-20260905/baseline`，运行旧版时同时用 LD_LIBRARY_PATH 固定旧 direct DSO，避免误混新旧库。

## 结果

完整 Streamcluster，单位秒。原基线来自[此前完整 suite](../flexguard-suite-20260905/README.md)，本轮新版本是三次完整运行，并非截取前段的诊断采样。

| 192 线程 | 原均值，3 次 | 新版三次 | 新均值 | 原均值 / 新均值 |
|---|---:|---|---:|---:|
| MCS-Accordin | 400.277 | 72.253 / 72.956 / 73.157 | 72.789 | 5.50× |
| MCS-TAS-Accordin | 364.112 | 77.981 / 72.902 / 73.586 | 74.823 | 4.87× |

FlexGuard 同配置既有三次均值为 178.838 s。本轮六个聚类输出均为 46,272 B，SHA-256 都是 `dfeea2357203cceeb8bcdac4984ffc9da9c953f1f1d19c06990626a4575ef01f`，与此前三种锁的九个输出相同。此检查证明结果与基线一致，不代替独立算法正确性证明。

192 线程、100 轮 PARSEC barrier，本轮每个配置三次均值。`perf stat` 统计整个进程，包含启动/回收；微基准较短，不解释小幅差异。

| 配置 | 耗时 s | sched_yield 次数 | CPU migrations | FUTEX_WAKE_PRIVATE 次数 |
|---|---:|---:|---:|---:|
| 原 MCS | 1.6290 | 341,699 | 56,042 | 37,971 |
| 新 MCS | 0.3934 | 240 | 931 | 38,010 |
| 原 MCS-TAS | 1.4049 | 672,191 | 49,464 | 38,017 |
| 新 MCS-TAS | 0.4628 | 322 | 647 | 38,010 |
| FlexGuard | 0.6964 | 0 | 12,981 | 18,286 |

新 MCS / MCS-TAS 的 task-clock 均值分别为 510 / 503 ms，原版为 105,054 / 58,508 ms。wake 系统调用总数基本不变，而无效 yield、自旋 CPU 消耗和迁移明显下降，符合“通知后停泊，实际 wake 前提交请求，沿用授权重获锁”的设计目标。这里测量的是方案 1+3 的组合效果，没有单独拆分两者贡献。后续 LevelDB 192 线程读写测量见 [readrandom / fillrandom 对比](../leveldb-relock-20260905/README.md)。

## 数据与复现

- [汇总](summary.json)、[Streamcluster 原始记录](stream.jsonl)、[barrier 原始记录](barrier.jsonl)；每次运行的 `.log` 和 barrier `.stat.csv` 同目录。
- [源码与 DSO 哈希](metadata.json)、[运行脚本](bench.py)。脚本依赖之前 suite 的 `target/flexguard-suite-20260905/run.py`、Streamcluster/FG 二进制及 `target/streamcluster-analysis-20260905/barrier`；这些的构建说明保留在相应历史报告。原版 DSO 快照位于 `target/cond-relock-20260905/baseline/`。
- [direct 测试](check-direct-final.log)、[LiTL 回归](check-regression.log)、[2 CPU BPF 测试](check-bpf-2cpu.log)。日志中保留实际命令与 backend 信息。

保留上述已构建基准和旧 DSO 快照后，从仓库根目录运行：

```sh
make litl
sudo python3 docs/benchmarks/cond-relock-20260905/bench.py barrier
sudo python3 docs/benchmarks/cond-relock-20260905/bench.py stream
```

脚本在 target 下写结果，重复运行会追加 JSONL；归档中的本轮数据为 15 个有效 barrier 运行和 6 个有效完整 Streamcluster 运行。历史 admission 关闭消融只作为[先前诊断](../streamcluster-analysis-20260905/README.md)保留，本轮没有关闭 admission。
