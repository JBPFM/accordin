# 实际构建与试运行

## LiTL 前端的完整矩阵（litl-full-20260909）

机器：Intel Xeon Gold 5318Y，2 socket × 24 核、无 SMT；CPU 0–47，P=48，内核 7.0.0-30，
线程数 12/24/48/96/192。六种锁全部经 LiTL `directlock`/`directcond` 前端接入，参数与
构建见 [README](README.md)。矩阵为 6 负载 × 6 锁 × 5 线程档 ×（1 次预热 + 3 次测量）
= 720 次尝试，随机顺序，seed=20260909，单个进程上限 600 秒。

### 命令与版本

先在 d6f3e95（`Describe what the six locks actually share`）上启动，本地 12:12：

```sh
sudo -n python3 experiments/run.py --profile full --allow-unsupported \
  --build target/experiments-litl --out target/results/litl-full-20260909
```

跑到第 446 次尝试时停下。停下的原因是运行中定下一条规则：一个配置只要有一次尝试
超时，该配置剩余的尝试就不再实测。驱动脚本按这条规则改写（2e4a876，
`Stop rerunning a configuration after one of its attempts times out`），本地 19:13 起
用同一个输出目录续跑，20:17 跑完 720 次尝试：

```sh
sudo -n python3 experiments/run.py --profile full --allow-unsupported \
  --build target/experiments-litl --out target/results/litl-full-20260909 --resume
```

因此 `results.jsonl` 的前 446 行没有 `driver_sha256` 字段，后 274 行都带同一个
`driver_sha256 = acb762112b72362f2a00988f98672d16c731d63ab3ab429c3520365693af305f`，
两段驱动可据此区分。`--resume` 不重测已记录的尝试，前 446 次尝试是在旧规则下实测的，
其中的超时点当时没有触发跳过。

同一天更早还有一次同样命令、输出到 `target/results/litl-full-20260909-aborted-gcr1`
的启动（本地 11:49），在第 2 次尝试后手动停止：当时 LiTL 的 GCR 还是 active_limit=1
的旧配置，论文参数由 ec8d247（`Give the LiTL GCR baseline the paper's thresholds`，
本地 12:11）才写入。该目录只有 1 个 ok、1 个 timeout，`summary.json` 里没有任何行，
不参与比较，也不要引用。

### 计数

`audit.json` 记 `expected_attempts=720`、`recorded_attempts=720`：

**ok 553、timeout 28、skipped 19、error 0、invalid 0、unsupported 120。**

120 个 unsupported 全部是 mcs-tse，`tse_probe` 判定内核不提供 rseq slice extension
（6 负载 × 5 线程档 × 4 次尝试）。28 次超时中 12 次落在预热、16 次落在测量；19 个
skipped 全部在测量阶段，是同配置已超时因而未执行的尝试。所有超时的 `wall_seconds`
都落在 600.005–600.077 秒，即 600 秒进程上限。

### 各负载中位吞吐

取 `summary.csv` 的 `median_rate`。**全超时**表示该配置 `completed=0`；带 † 的单元格
`timeouts>0` 且 `completed>0`，中位数只由剩下的成功样本得出。`skipped` 列是该锁在五个
线程档上被跳过的尝试总数。

**LevelDB readrandom**（ops/s）

| 锁 | 12 | 24 | 48 | 96 | 192 | skipped |
|---|---:|---:|---:|---:|---:|---:|
| mcs | 2.18M | 2.54M | 946.8k | 4.9k † | **全超时** | 3 |
| mcs-tas | 1.68M | 1.73M | 1.10M | 566.6k | 412.8k | 0 |
| gcr | 917.7k | 997.0k | 1.05M | 1.06M | 960.8k | 0 |
| flexguard | 1.62M | 1.81M | 1.18M | 814.8k | 855.6k | 0 |
| mcs-tse | unsupported | unsupported | unsupported | unsupported | unsupported | 0 |
| accordin | 1.31M | 1.37M | 1.32M | 1.29M | 1.29M | 0 |

**LevelDB fillrandom**（ops/s）

| 锁 | 12 | 24 | 48 | 96 | 192 | skipped |
|---|---:|---:|---:|---:|---:|---:|
| mcs | 309.5k | 120.9k | 57.3k | 2.2k | **全超时** | 1 |
| mcs-tas | 252.7k | 105.7k | 94.6k | 10.3k | 2.4k | 0 |
| gcr | 293.5k | 85.0k | 50.3k | 30.7k | 21.8k | 0 |
| flexguard | 307.5k | 140.9k | 128.8k | 127.6k | 107.1k | 0 |
| mcs-tse | unsupported | unsupported | unsupported | unsupported | unsupported | 0 |
| accordin | 202.5k | 196.3k | 202.5k | 207.9k | 207.5k | 0 |

**Kyoto CacheDB**（ops/s）

| 锁 | 12 | 24 | 48 | 96 | 192 | skipped |
|---|---:|---:|---:|---:|---:|---:|
| mcs | 7.17M | 6.88M | 2.13M | 13.9k | 4.2k | 0 |
| mcs-tas | 7.61M | 7.70M | 3.87M | 2.64M | 1.72M | 0 |
| gcr | 6.69M | 6.71M | 6.06M | 5.69M | 5.86M | 0 |
| flexguard | 7.32M | 7.62M | 7.07M | 6.28M | 6.86M | 0 |
| mcs-tse | unsupported | unsupported | unsupported | unsupported | unsupported | 0 |
| accordin | 4.12M | 4.84M | 4.06M | 4.05M | 4.06M | 0 |

**RocksDB readrandom**（ops/s）

| 锁 | 12 | 24 | 48 | 96 | 192 | skipped |
|---|---:|---:|---:|---:|---:|---:|
| mcs | 875.8k | 941.8k | 517.1k | **全超时** | **全超时** | 3 |
| mcs-tas | 889.9k | 990.1k | 712.8k | 372.3k | 253.5k | 0 |
| gcr | 588.7k | 659.1k | 685.4k | 782.6k | 762.2k | 0 |
| flexguard | 898.4k | 927.7k | 728.7k | 508.4k | 521.2k | 0 |
| mcs-tse | unsupported | unsupported | unsupported | unsupported | unsupported | 0 |
| accordin | 798.6k | 781.7k | 765.2k | 693.9k | 686.7k | 0 |

**Streamcluster**（runs/s）

| 锁 | 12 | 24 | 48 | 96 | 192 | skipped |
|---|---:|---:|---:|---:|---:|---:|
| mcs | 0.1288 | 0.09960 | 0.03839 | **全超时** | **全超时** | 3 |
| mcs-tas | 0.1252 | 0.09496 | 0.05369 | **全超时** | **全超时** | 4 |
| gcr | 0.1218 | 0.09214 | 0.01776 | 0.004065 | **全超时** | 2 |
| flexguard | 0.1280 | 0.09390 | 0.08567 | 0.01521 | 0.002976 | 0 |
| mcs-tse | unsupported | unsupported | unsupported | unsupported | unsupported | 0 |
| accordin | 0.04733 | 0.05339 | 0.04606 | 0.03950 | 0.02887 | 0 |

**Raytrace**（runs/s）

| 锁 | 12 | 24 | 48 | 96 | 192 | skipped |
|---|---:|---:|---:|---:|---:|---:|
| mcs | 1.029 | 1.937 | 0.6574 | **全超时** | **全超时** | 3 |
| mcs-tas | 1.033 | 1.934 | 0.9629 | 0.1998 | 0.1223 | 0 |
| gcr | 1.024 | 1.922 | 1.939 | 1.841 | 1.959 | 0 |
| flexguard | 1.028 | 1.944 | 2.991 | 2.486 | 2.462 | 0 |
| mcs-tse | unsupported | unsupported | unsupported | unsupported | unsupported | 0 |
| accordin | 1.023 | 1.910 | 2.849 | 2.884 | 2.882 | 0 |

### 2P / 4P 的保留比例与相对 MCS 的加速

`retention_vs_P` 是该锁在该线程档相对自身 48 线程（P）的中位吞吐比例，
`speedup_vs_mcs` 是同负载同线程档相对 MCS 的中位吞吐比例。mcs-tse 整列 unsupported，
未列入。空格（—）的含义：`retention_vs_P` 为空表示该锁在该点没有中位数（全部尝试
超时）；`speedup_vs_mcs` 为空表示该点 MCS 没有中位数，没有基线可除。

**96 线程（2P）retention_vs_P / speedup_vs_mcs**

| 负载 | mcs | mcs-tas | gcr | flexguard | accordin |
|---|---:|---:|---:|---:|---:|
| LevelDB readrandom | 0.00518 / 1.00 | 0.517 / 115.5 | 1.01 / 215.8 | 0.693 / 166.1 | 0.983 / 263.5 |
| LevelDB fillrandom | 0.0384 / 1.00 | 0.109 / 4.70 | 0.611 / 14.0 | 0.991 / 58.0 | 1.03 / 94.5 |
| Kyoto CacheDB | 0.00653 / 1.00 | 0.682 / 189.4 | 0.938 / 408.5 | 0.889 / 451.2 | 0.997 / 290.7 |
| RocksDB readrandom | — / — | 0.522 / — | 1.14 / — | 0.698 / — | 0.907 / — |
| Streamcluster | — / — | — / — | 0.229 / — | 0.178 / — | 0.857 / — |
| Raytrace | — / — | 0.207 / — | 0.950 / — | 0.831 / — | 1.01 / — |

**192 线程（4P）retention_vs_P / speedup_vs_mcs**

| 负载 | mcs | mcs-tas | gcr | flexguard | accordin |
|---|---:|---:|---:|---:|---:|
| LevelDB readrandom | — / — | 0.377 / — | 0.918 / — | 0.728 / — | 0.980 / — |
| LevelDB fillrandom | — / — | 0.0253 / — | 0.434 / — | 0.831 / — | 1.02 / — |
| Kyoto CacheDB | 0.00196 / 1.00 | 0.446 / 412.9 | 0.968 / 1405.0 | 0.971 / 1643.8 | 1.00 / 973.5 |
| RocksDB readrandom | — / — | 0.356 / — | 1.11 / — | 0.715 / — | 0.897 / — |
| Streamcluster | — / — | — / — | — / — | 0.0347 / — | 0.627 / — |
| Raytrace | — / — | 0.127 / — | 1.01 / — | 0.823 / — | 1.01 / — |

### 观察

- 4P（192 线程）时 MCS 只在 Kyoto CacheDB 完成，且只剩 P 点的 0.196%；LevelDB
  readrandom/fillrandom、RocksDB、Streamcluster、Raytrace 的 MCS 4P 全部超时。2P
  （96 线程）时 RocksDB、Streamcluster、Raytrace 的 MCS 也全部超时，CacheDB 保留 0.653%、
  fillrandom 3.84%、readrandom 0.518%。
- mcs-tas 除 Streamcluster 外在 2P/4P 都能完成，但保留比例塌到 fillrandom 4P 的 0.0253、
  Raytrace 4P 的 0.127、CacheDB 4P 的 0.446。
- GCR 跑的是论文配置，日志里为
  `gcr_mcs: effective_config active_limit=4 rejoin_limit=2 signal_period=16384 (0x4000) passive_spins=1024`。
  它在 RocksDB 和 Raytrace 的 2P/4P 保留比例接近或超过 1（1.14/1.11、0.950/1.01），
  在 LevelDB fillrandom 4P 掉到 0.434，在 Streamcluster 48 线程已降到 0.01776 runs/s、
  96 线程 0.004065 runs/s、192 线程全部超时。
- FlexGuard 在 Raytrace 48 线程取得全矩阵最高值 2.991 runs/s，但在 Streamcluster 的
  2P/4P 保留比例只有 0.178 / 0.0347。
- Accordin 在低线程档慢于 baseline。P/4（12 线程）相对 MCS：Streamcluster 0.37×、
  CacheDB 0.58×、LevelDB readrandom 0.60×、LevelDB fillrandom 0.65×、RocksDB 0.91×、
  Raytrace 0.99×。P/2（24 线程）相对 MCS：Streamcluster 0.54×、readrandom 0.54×、
  CacheDB 0.70×、RocksDB 0.83×、Raytrace 0.99×，只有 fillrandom 是 1.62×。
  同两档相对 FlexGuard：readrandom 0.81×/0.76×、CacheDB 0.56×/0.63×、
  Streamcluster 0.37×/0.57×、RocksDB 0.89×/0.84×；相对 GCR：readrandom 1.42×/1.37×、
  CacheDB 0.62×/0.72×、Streamcluster 0.39×/0.58×、RocksDB 1.36×/1.19×。
- Accordin 的保留比例在 2P/4P 是六个负载里最平的：readrandom 0.983/0.980、
  fillrandom 1.03/1.02、CacheDB 0.997/1.00、RocksDB 0.907/0.897、Raytrace 1.01/1.01、
  Streamcluster 0.857/0.627。但平不等于最快：CacheDB 的绝对吞吐在全部五个线程档都低于
  FlexGuard 和 GCR（4P 为 0.59× / 0.69×），RocksDB 2P/4P 也低于 GCR（0.89× / 0.90×）。
- Streamcluster 的 96/192 线程：mcs 与 mcs-tas 两档都全部超时，gcr 只在 192 线程全部
  超时（96 线程三次测量都完成，0.004065 runs/s）。FlexGuard 与 Accordin 两档都没有超时。
- 超时是 600 秒进程上限，不是死锁判定。超时配置的剩余尝试记为 `skipped` 且不再执行，
  所以这些点的样本数少于 3，中位数是成功样本的条件统计。全矩阵只有
  `leveldb-readrandom / mcs / 96` 属于部分超时（completed=1、timeouts=1、skipped=1），
  它的 4.9k ops/s 只来自一个样本，必须连同超时次数一起读。
- MCS 在 48 线程的 Raytrace 与 Streamcluster 三次测量离散度极大（cv 0.936 / 0.613，
  Raytrace 从 0.0129 到 1.135 runs/s），这两个中位数不宜单独引用。

### 图与审计

`python3 experiments/plot.py target/results/litl-full-20260909` 生成
`throughput.png` 与 `retention.png`（结果目录属 root，需要 sudo 才能写入）。
结果目录和图都不入库。

驱动输出的最后一行是计数
`{"ok": 553, "timeout": 28, "skipped": 19, "error": 0, "invalid": 0, "unsupported": 120}`；
收尾审计写在 `audit.json`：

```json
"artifacts_unchanged": true,
"sched_ext_final": "disabled",
```

即所有被测文件哈希不变，退出时 sched_ext 处于 disabled。

本地原始证据（属 root）在 `target/results/litl-full-20260909/`：`config.json`、
`manifest.json`、`preflight.json`、`host.json`、`results.jsonl`、`summary.csv`、
`summary.json`、`audit.json`，以及 `logs/`、`outputs/`。

## LiTL 统一前端后的 smoke

机器：Intel Xeon Gold 5318Y，2 socket × 24 核、无 SMT；CPU 0–47，P=48，
线程数 12/24/48/96/192。六种锁全部经 LiTL `directlock`/`directcond` 前端接入。

```sh
python3 experiments/prepare.py --jobs 12 --build target/experiments-litl
sudo -n python3 experiments/run.py --profile smoke --build target/experiments-litl \
  --out target/results/litl-smoke-20260909
```

180 个配置全部尝试：**136 个有效样本、14 次超时、30 个 unsupported、0 个 error/invalid**。
运行前的绑定检查：mcs/mcs-tas/gcr 无 BPF、sched_ext enable_seq 不变；flexguard 观察到
BPF fd、enable_seq 不变；accordin 观察到 direct 库、BPF fd 且 enable_seq 恰好加一。
mcs-tse 由 `tse_probe` 判定内核不提供 rseq slice extension，30 个配置全部记为
`unsupported`。退出时 sched_ext=disabled，所有被测文件哈希不变。

| 锁 | ok | timeout |
|---|---|---|
| mcs | 23 | 7 |
| mcs-tas | 28 | 2 |
| gcr | 27 | 3 |
| flexguard | 28 | 2 |
| mcs-tse | 0 | 0（30 unsupported） |
| accordin | 30 | 0 |

超时集中在 Streamcluster 和 Raytrace 的 96/192 线程，属于 30 s smoke 预算内未完成，
不是死锁判定；短样本不作为性能排名。

## 2026-09-09 更早的一轮（旧构建路径）

机器：TaiShan-v110，96 物理核、无 SMT；CPU 0–95，线程数 24/48/96/192/384。
构建使用当前工作区（包含运行前已有的未提交 Accordin 修改）；精确版本、源文件与动态库
哈希保存在每个结果目录的 `manifest.json`，不能只用 Git HEAD 替代这份源码记录。

## 完整五档 smoke

```sh
sudo -n python3 experiments/run.py --profile smoke --timeout 20 \
  --out results/overload-smoke-20260909
```

180 个配置全部尝试：**133 个有效样本、17 次超时、30 个 unsupported、0 个 error/invalid**。
六个负载、五个支持的锁、五档线程均实际执行。MCS-TSE 保留全部 30 个配置点，未以其他
实现代替。内核/用户态缺少 rseq slice-extension 支持；独立 `PR_RSEQ_SLICE_EXTENSION_GET`
检查也返回 `EINVAL`。

运行前五个支持后端都通过真实符号绑定、8 线程共享计数、嵌套 mutex、条件变量唤醒检查。
所有有效应用样本通过计时/工作量/相应输出检查；Accordin 的 BPF 和 sched_ext 加载得到
实际观测。退出时 sched_ext=disabled，所有被测文件哈希不变。

| workload | 20 秒上限内未完成的配置 |
|---|---|
| LevelDB readrandom | MCS 192 |
| LevelDB fillrandom | MCS 384、MCS-TAS 384 |
| Streamcluster | MCS 96/192/384；MCS-TAS 192/384；GCR 96/192/384；FlexGuard 192/384；Accordin 384 |
| Raytrace | MCS 96/192/384 |
| CacheDB | 无 |
| RocksDB | 无 |

这里的超时不是死锁判定。尤其 Streamcluster 的 smoke 缩小了点数与维度，却保留了大量
barrier，同步占比与正式配置不同。Accordin 在这个 4P 配置仍然超时，不能声称所有应用都
已消除性能崩溃。短样本的启动、调度和缓存扰动也较大，不将这些单样本作为正式排名。

后续已对 Accordin/Streamcluster 384 线程超时做性能采样、阶段计数和重复复现：
原二进制三次均完成，ROI 为 45.281/41.028/28.882 秒，仍全部超过原 20 秒预算。
主要开销来自近万轮两阶段 barrier 的约 746 万次等待及广播释放。
两个 BPF 修改和配置对照均未验证出能消除超时的修复，策略修改已撤回。
完整证据和复现入口见 [Streamcluster 超时定位](STREAMCLUSTER_TIMEOUT.md)。

本地原始证据：
[配置](../results/overload-smoke-20260909/config.json)、
[逐次记录](../results/overload-smoke-20260909/results.jsonl)、
[CSV 汇总](../results/overload-smoke-20260909/summary.csv)、
[运行前校验](../results/overload-smoke-20260909/preflight.json)、
[最终审计](../results/overload-smoke-20260909/audit.json)、
[吞吐量图](../results/overload-smoke-20260909/throughput.png)、
[相对 P 的保留比例](../results/overload-smoke-20260909/retention.png)。

## 正式工作量的补充试跑

进一步运行默认 full 数据库工作量，比较 P=96 和 4P=384 的 MCS/Accordin，每点一次、
无额外预热轮、每进程 45 秒上限；每个随机读进程内仍执行 cache 预热。

```sh
sudo -n python3 experiments/run.py --profile full --threads 96 384 --locks mcs accordin \
  --workloads leveldb-readrandom leveldb-fillrandom kyoto-cachedb rocksdb raytrace \
  --repeats 1 --warmups 0 --timeout 45 --out results/overload-full-pilot-20260909
```

**15 个有效样本、5 次超时、0 个 error/invalid**。下表为应用 ROI 秒数，越低越好。
“超时”是整个进程达到 45 秒预算，不是假设 ROI 恰好为 45 秒，也不转换成零吞吐量。

| workload | MCS 96 | Accordin 96 | MCS 384 | Accordin 384 |
|---|---:|---:|---|---:|
| LevelDB readrandom，1,966,080 reads | 31.824 | 2.271 | 超时 | 2.261 |
| LevelDB fillrandom，983,040 puts | 14.839 | 7.469 | 超时 | 8.467 |
| CacheDB，1,966,080 ops | 1.790 | 0.989 | 超时 | 1.001 |
| RocksDB readrandom，1,966,080 reads | 31.339 | 3.477 | 超时 | 3.519 |
| Raytrace，FlexGuard 原配置 | 3.449 | 0.592 | 超时 | 0.621 |

这些单次结果支持继续用此参数做正式重复实验：Accordin 在这五项的 4P 点保有有效进度，
MCS 未在预算内完成。但这不是其他 baseline 的 full 比较，不构成普遍优越性或统计显著性
证明，也没有独立测得每个实际临界区的持有时间分布。观察器和后台系统活动未完全隔离。

另外，以 Accordin、24 个线程执行 **FlexGuard 原始 Streamcluster 参数**：

```sh
sudo -n python3 experiments/run.py --profile full --threads 24 --locks accordin \
  --workloads streamcluster --repeats 1 --warmups 0 --timeout 180 \
  --out results/streamcluster-full-pilot-20260909
```

完整运行成功，ROI **23.999 秒**；输出维度 512、聚类权重总和 32768，通过输出校验。
这验证正式参数可以运行；尚未完成 Streamcluster full 工作量的全部锁/线程矩阵。

两轮补充试跑结束时也均确认文件哈希不变、sched_ext=disabled。本轮最终验证共记录
**201 个配置尝试：149 个有效样本、22 次超时、30 个 unsupported、0 个 error/invalid**，
不包含前置定位问题的 pilot 和独立锁校验。

证据：[full 逐次记录](../results/overload-full-pilot-20260909/results.jsonl)、
[full 汇总](../results/overload-full-pilot-20260909/summary.csv)、
[full 审计](../results/overload-full-pilot-20260909/audit.json)、
[Streamcluster 记录](../results/streamcluster-full-pilot-20260909/results.jsonl)、
[Streamcluster 审计](../results/streamcluster-full-pilot-20260909/audit.json)。

## 兼容性问题的定位

前置 pilot 的失败记录保留在 `results/overload-pilot*-20260909/`，未混入最终 smoke：

- `taskset` 继承 LD_PRELOAD 会使 Accordin 在 exec 前后加载两次。最终脚本通过
  `taskset ... env LD_PRELOAD=... application` 将注入限定到应用，检查 enable_seq +1。
- FlexGuard 系列 timedwait 会直接 `exit(1)`。RocksDB writable/CompactedDB 的后台定时
  线程触发该分支，退出过程中主线程可见已释放的参数字符串，表现为乱码 benchmark 名称。
  GDB 抓到 `Timedwait not supported yet.`；最终采用所有锁统一的
  `readonly=1, open_files=1000` 普通只读缓存测试，不把兼容性退出算作锁崩溃。
- 旧归档 ARM patch 有两处缺失换行标记，已修复新副本；新脚本使用私有 LiTL/应用/锁构建
  目录，避免覆盖旧的 root 构建文件。

`python3 -m unittest discover -s experiments -p 'test_*.py' -v` 的五个检查通过，涵盖
聚合吞吐量及固定操作数、超时不能记成零、CPU 列表解析、RLE 截断和进程超时回收。
