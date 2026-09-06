# simplify：迁移 fullhook 的自适应条件等待

来源为 [fullhook-admission 的 f7fedc9](https://github.com/JBPFM/accordin/commit/f7fedc94c46ff98f3ad20ada1fecc414b3fe233a)，基线为 `simplify@934bd1c`。本轮从远端获取该提交，并将本地 fullhook-admission 快进到该版本；工作区始终在 simplify 上实现迁移。

## 实现与迁移边界

- BPF 发布 NORMAL_DSQ 与 WAITING_DSQ 是否有排队任务的提示，只在提示值改变时写入。消费者只需判断是否为零，无需每次 enqueue/dispatch 都写全局队列长度；该提示仍是近似信息，不参与锁正确性的判定。CV 等待者尝试一次 admission，未获准时仍能通过普通队列恢复运行并停泊。只有 CV 请求可以在这次 yield 中借用当前 CPU 的空名额；普通锁的续用/排队规则不变。
- 每个 condvar 的失败评分范围为 0–8；获准自旋未等到通知加 2，等到通知减 1。评分达到 3 时，排队需求会阻止或结束自旋。任何评分下，预算耗尽或失去 admission 名额都会结束自旋。被抢占的 CV 自旋者归还名额，已进入 raw lock 队列的线程继续保留名额。
- CV 自旋、通知发布和 relock 使用同一 epoch。通知与自旋退出的更新使用 CAS/发布协议，退出不能覆盖通知已发布的 WAITING。
- 自旋观察逻辑通知，重获锁仍遵循 simplify 的 mutex 接力队列。被通知但尚未获得接力权的线程停泊，不占用 CPU 等接力。signal/broadcast、取消补偿、超时后重获锁、嵌套等待均保留。
- waiter 的 parked 发布与 wake 发布使用顺序一致性握手：等待者先看到 wake，或者通知者看到 parked 并执行 futex wake。正在自旋的接力者可以省去唤醒 syscall。没有 shadow mutex，也没有 COND_VAR/COND_VAE 开关。
- 保留 simplify 的 raw lock 算法、外部锁存储和普通调度策略；未迁入 fullhook 的 holder 抢占、idle 直接投递、普通等待者 self-admit、TTAS/内嵌锁或 initial-exec TLS。

## 默认预算：MCS 为 0，MCS-TAS 为 50 µs

直接使用 fullhook 的 1000 µs 默认预算，在 MCS-TAS 的 LiTL 接力路径上造成明显 fillrandom 回退。关闭自旋后吞吐量恢复，50 µs 控制则优于关闭自旋；逻辑通知与接力分离也不能单独消除长预算的损失。因此保留队列需求与历史评分机制，为 MCS-TAS 选择 50 µs，并通过独立的三轮确认检查其取舍。

本次 LevelDB 源码的 `DBImpl::Write` 每次在栈上构造包含独立 condvar 的 Writer；这一点限制了每对象历史评分的复用，长预算的首次尝试成本仍可能反复发生。写入 leader 也需要在 mutex 外写日志和更新 memtable。结合预算对照，自旋占用 admission/CPU 是性能变化的合理解释；本轮没有新增线程级 profiling，不将其表述为已精确分摊的热点。

在同为 50 µs 的控制中，禁用“失败历史 + 排队需求”判断的 `balanced-fixed` 跑 streamcluster 为 100.90 秒，启用判断的 `balanced` 三轮均值为 78.57 秒。这个单次消融支持保留历史机制，但不足以精确估计其收益大小。

末尾复查时，MCS 的 fillrandom 基线从早期三轮均值 69.6 Kops/s 上升到 75.9 Kops/s，已与 50 µs 候选的 75.6 Kops/s 相当；同时 MCS 的 streamcluster 有明显耗时成本。因此不将这个不稳定的写入收益作为默认启用的理由，MCS 默认使用 0 µs，另行三轮检查；机制仍保留，可显式设置非零预算启用。

MCS-TAS 的三轮 fillrandom 为 187.9 Kops/s，最后新构建复查为 181.6 Kops/s，均高于早期基线三轮均值 159.9 Kops/s 和末尾单次 156.4 Kops/s。streamcluster 则仍有代价：相同 50 µs 路径三轮为 76.79 秒，最后新构建单次为 83.56 秒，而基线三轮为 75.34 秒、末尾单次为 72.42 秒。最后这一较慢样本完整保留；目前不能宣称 streamcluster 无回退，也不能将约 2% 的三轮差异当作稳定上界。

`ACCORDIN_CV_SPIN_US` 接受 0–1000000，MCS 默认 0、MCS-TAS 默认 50；0 只关闭自旋。无 BPF、禁用 admission、stats-only 或嵌套等待时不借用 CV 自旋名额。不同机器和不同等待分布可能需要不同预算。

## 配置与审计

- 192 worker、CPU 0–95、BPF/admission 启用、统计关闭；性能运行和编译使用 `/tmp/mutexbench-sweep-multi-lock.lock` 串行。
- LevelDB 使用与前次归因相同的二进制 SHA256 `5f4ee4e128e60af6ff13a434d82c39bfee52ff80dc10d815f5a6f60b93034171`，100 万键种子、100 B value、30 秒、tmpfs。每次读复制并核对种子，每次写使用新数据库。吞吐量取 BENCH_TOTAL 总操作数 / 合并墙钟时间。
- 完整 streamcluster 参数：`10 30 512 32768 32768 2000 none <output> 192`。程序计时除以 100000000 得秒；每次输出核对 SHA256 `dfeea2357203cceeb8bcdac4984ffc9da9c953f1f1d19c06990626a4575ef01f`。
- 每次核对实际加载库、BPF fd、sched_ext 状态和 enable_seq。baseline、balanced、pressure、defaults 和控制版本都是独立源码/构建，元数据记录源码及动态库哈希。工作区生产源码与 defaults 快照一致，并另外完成工作区重建检查。defaults 相对 pressure 只调整 MCS 的默认预算为 0，TAS 的 50 µs 行为不变。
- 表中的预算是 runner 传入的环境参数；baseline 没有这个参数的解析或 CV 自旋实现，始终使用原停泊路径。应用测试显式传入所选预算，正确性检查还覆盖未设置预算变量的后端默认值。继承的 runner 中有旧 fullhook arm 入口，本次没有运行该入口；来源机制以归档的 f7fedc9 补丁为准。
- 探索是单次控制。第一阶段 baseline 与 balanced 交错各测三轮；将需求提示改为布尔值并减少共享写入后，pressure 在后续阶段独立各测三轮，再在末尾补测基线各一次检查时间漂移。最终 MCS 默认 0 µs 使用新构建三轮数据；TAS 使用 pressure 相同 50 µs 路径的三轮数据，并对新构建再复查一次。主表不混入探索或末尾复查值。因此最终版本与基线不是逐轮配对测量，几个百分点的差异应结合逐轮波动及末尾基线复查看待。原始数据保留失败标记，不丢弃回退样本。

## 正确性检查

以下命令在最终工作区全部通过：

```sh
make -j8 check check-litl
sudo make check-bpf check-litl-bpf
sudo taskset -c 0,1 env LITL_TEST_THREADS=24 LITL_TEST_ITERATIONS=1000 \
  make check-bpf check-litl-bpf
sudo env ACCORDIN_CV_SPIN_US=50 make check-bpf check-litl-bpf
```

覆盖直接 ABI、C/C++ LiTL、首次 condvar 等待、signal/broadcast、取消补偿、超时和 clockwait、多个 CV 共用 mutex、嵌套等待及 NDEBUG 无 shadow mutex。新增 `check-spin` 对真实 direct 自旋函数注入确定性调度事件，检查排队需求/评分、失去名额、通知与退出竞争、逻辑通知先于接力、同 epoch 重获锁、过期 deadline 与嵌套跳过。公开头文件也通过严格 C11 语法检查。

## 原始材料与复现

- `production.patch` 是迁入补丁；其余候选 patch 仅是归因材料。`upstream-commit.txt`/`upstream.patch` 保存来源。
- `results.csv`、`summary.json` 和各阶段 JSON 保存结果；`logs.tar.gz` 保存应用日志、streamcluster 输出、构建和检查日志；`variant-metadata.tar.gz` 保存独立候选的源码/库哈希。
- runner 复用仓库已有的 `docs/benchmarks/leveldb-branches-20260906/run.py`。将本目录 Python 脚本复制到 `target/cv-adaptive-20260906/` 后运行。已有候选和阶段不应覆盖；需要相同的应用二进制与种子。`build.py` 拒绝覆盖已有候选。

```sh
python3 target/cv-adaptive-20260906/build.py baseline
python3 target/cv-adaptive-20260906/build.py defaults \
  --patch docs/benchmarks/cv-adaptive-20260906/production.patch
sudo python3 target/cv-adaptive-20260906/budget.py --stage repeat-mcs-leveldb \
  --arms baseline defaults --backends mcs_accordin --spin-us 0 --repeats 3
sudo python3 target/cv-adaptive-20260906/budget.py --stage repeat-tas-leveldb \
  --arms baseline defaults --backends mcs_tas_accordin --spin-us 50 --repeats 3
sudo python3 target/cv-adaptive-20260906/regression_budget.py --stage repeat-mcs-stream \
  --arms baseline defaults --backends mcs_accordin --modes stream --spin-us 0 --repeats 3
sudo python3 target/cv-adaptive-20260906/regression_budget.py --stage repeat-tas-stream \
  --arms baseline defaults --backends mcs_tas_accordin --modes stream --spin-us 50 --repeats 3
```

## 所选预算的三轮结果

| 工作负载 | 后端 / 预算 | 原基线三轮均值 | 末尾基线单次 | 当前三轮均值 | 相对原基线 |
| --- | --- | ---: | ---: | ---: | ---: |
| readrandom | mcs_accordin / 0 µs | 625.521 Kops/s | 622.388 Kops/s | 610.181 Kops/s | -2.45% |
| readrandom | mcs_tas_accordin / 50 µs | 960.974 Kops/s | 932.529 Kops/s | 949.554 Kops/s | -1.19% |
| fillrandom | mcs_accordin / 0 µs | 69.581 Kops/s | 75.881 Kops/s | 73.434 Kops/s | +5.54% |
| fillrandom | mcs_tas_accordin / 50 µs | 159.864 Kops/s | 156.397 Kops/s | 187.886 Kops/s | +17.53% |
| stream | mcs_accordin / 0 µs | 74.935 秒 | 76.216 秒 | 74.824 秒 | -0.15% |
| stream | mcs_tas_accordin / 50 µs | 75.340 秒 | 72.416 秒 | 76.789 秒 | +1.92% |

吞吐量越高越好；streamcluster 耗时越低越好。变化为当前/原基线 − 1，不把探索样本混入确认均值。MCS 写入基线在末尾明显上移，不能把相对早期基线的百分比当作稳定收益；因此 MCS 默认关闭自旋。TAS 三轮数据来自相同 50 µs 路径的 pressure 构建，调整 MCS 默认值后的 defaults 构建另有一次完整应用复查。

### 逐轮数据

| 工作负载 | 后端 | 版本 | 三轮数值 | 标准差/均值 |
| --- | --- | --- | --- | ---: |
| readrandom | mcs_accordin | baseline | 625.583 / 629.629 / 621.353 | 0.66% |
| readrandom | mcs_accordin | current | 626.441 / 625.263 / 578.840 | 4.45% |
| readrandom | mcs_tas_accordin | baseline | 938.448 / 956.638 / 987.836 | 2.60% |
| readrandom | mcs_tas_accordin | current | 938.365 / 947.372 / 962.925 | 1.31% |
| fillrandom | mcs_accordin | baseline | 68.351 / 70.918 / 69.474 | 1.85% |
| fillrandom | mcs_accordin | current | 77.124 / 66.849 / 76.328 | 7.78% |
| fillrandom | mcs_tas_accordin | baseline | 160.885 / 159.501 / 159.206 | 0.56% |
| fillrandom | mcs_tas_accordin | current | 190.858 / 182.837 / 189.962 | 2.34% |
| stream | mcs_accordin | baseline | 74.906 / 75.551 / 74.348 | 0.80% |
| stream | mcs_accordin | current | 77.177 / 73.183 / 74.111 | 2.79% |
| stream | mcs_tas_accordin | baseline | 76.378 / 76.321 / 73.321 | 2.32% |
| stream | mcs_tas_accordin | current | 76.667 / 76.766 / 76.932 | 0.17% |

## 调整默认值后的 TAS 构建复查

以下为 defaults 新构建的单次结果，不并入 pressure 的三轮均值。两者 TAS 路径和预算相同。

| 工作负载 | 单次结果 |
| --- | ---: |
| readrandom | 924.575 Kops/s |
| fillrandom | 181.568 Kops/s |
| stream | 83.559 秒 |

## 未默认启用的 MCS 50 µs 候选

保留 pressure 的完整三轮，说明为何将 MCS 的默认预算改回 0。写入结果与末尾基线相当，streamcluster 耗时则更高。

| 工作负载 | 三轮数值（Kops/s 或秒） | 均值 |
| --- | --- | ---: |
| readrandom | 629.807 / 620.902 / 629.382 | 626.697 |
| fillrandom | 77.352 / 77.468 / 71.832 | 75.551 |
| stream | 83.404 / 80.091 / 78.144 | 80.546 |

## 需求提示优化之前的配对确认

`balanced` 发布队列长度，`pressure` 使用最终的队列非空提示，两个后端均为 50 µs。下表仅使用第一阶段基线与 balanced 交错测量的三轮数据。

| 工作负载 | 后端 | 基线 | balanced | 变化 |
| --- | --- | ---: | ---: | ---: |
| readrandom | mcs_accordin | 625.521 | 636.366 | +1.73% |
| readrandom | mcs_tas_accordin | 960.974 | 957.653 | -0.35% |
| fillrandom | mcs_accordin | 69.581 | 76.776 | +10.34% |
| fillrandom | mcs_tas_accordin | 159.864 | 180.320 | +12.80% |
| stream | mcs_accordin | 74.935 | 79.478 | +6.06% |
| stream | mcs_tas_accordin | 75.340 | 78.570 | +4.29% |

## 探索与补充对照

下表均为单次探索或末尾基线复查，不混入三轮确认均值。`adaptive`/`fixed` 是初版等待接力 wake 的实现；`notified`/`balanced` 以逻辑通知结束 CV 自旋。`fixed` 和 `balanced-fixed` 只禁用按历史评分响应队列压力的判断，仍保留预算和抢占撤销。

| 阶段 | 工作负载 | 后端 | 版本 | 自旋上限 µs | 数值（Kops/s 或秒） | 有效 |
| --- | --- | --- | --- | ---: | ---: | --- |
| closing-baseline-leveldb | readrandom | mcs_accordin | baseline | 50 | 622.388 | True |
| closing-baseline-leveldb | readrandom | mcs_tas_accordin | baseline | 50 | 932.529 | True |
| closing-baseline-leveldb | fillrandom | mcs_accordin | baseline | 50 | 75.881 | True |
| closing-baseline-leveldb | fillrandom | mcs_tas_accordin | baseline | 50 | 156.397 | True |
| closing-baseline-stream | stream | mcs_accordin | baseline | 50 | 76.216 | True |
| closing-baseline-stream | stream | mcs_tas_accordin | baseline | 50 | 72.416 | True |
| default-tas-leveldb | readrandom | mcs_tas_accordin | defaults | 50 | 924.575 | True |
| default-tas-leveldb | fillrandom | mcs_tas_accordin | defaults | 50 | 181.568 | True |
| default-tas-stream | stream | mcs_tas_accordin | defaults | 50 | 83.559 | True |
| fixed50-stream | stream | mcs_tas_accordin | balanced-fixed | 50 | 100.904 | True |
| screen-leveldb | fillrandom | mcs_tas_accordin | baseline | 1000 | 161.417 | True |
| screen-leveldb | fillrandom | mcs_tas_accordin | adaptive | 1000 | 45.811 | True |
| screen-leveldb | fillrandom | mcs_tas_accordin | fixed | 1000 | 46.243 | True |
| screen-notified | fillrandom | mcs_tas_accordin | notified | 1000 | 65.770 | True |
| screen-notified100 | fillrandom | mcs_tas_accordin | notified | 100 | 182.099 | True |
| screen-notified50 | fillrandom | mcs_tas_accordin | notified | 50 | 188.362 | True |
| screen-pressure | stream | mcs_accordin | pressure | 50 | 77.522 | True |
| screen-pressure | stream | mcs_tas_accordin | pressure | 50 | 76.012 | True |
| screen-spin0 | fillrandom | mcs_tas_accordin | adaptive | 0 | 159.970 | True |
| screen-spin10 | fillrandom | mcs_tas_accordin | adaptive | 10 | 164.798 | True |
| screen-spin50 | fillrandom | mcs_tas_accordin | adaptive | 50 | 191.628 | True |
| screen-stream | stream | mcs_tas_accordin | baseline | 1000 | 76.729 | True |
| screen-stream | stream | mcs_tas_accordin | adaptive | 1000 | 95.282 | True |
| screen-stream | stream | mcs_tas_accordin | fixed | 1000 | 250.641 | True |
| screen-stream0 | stream | mcs_accordin | balanced | 0 | 77.159 | True |
| screen-stream0 | stream | mcs_tas_accordin | balanced | 0 | 78.383 | True |
| screen-stream50 | stream | mcs_tas_accordin | notified | 50 | 80.789 | True |

记录生成时间：2026-09-06T08:29:47.387824+00:00。已归档 90 次应用运行，其中 90 次有效。
