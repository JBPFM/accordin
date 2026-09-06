# LevelDB readrandom / fillrandom：192 线程，condvar 方案 1+3

使用当前标准 LiTL 的 `mcs_accordin`、`mcs_tas_accordin` 与 FlexGuard，六个配置各重新运行三次，共 18 个有效性能样本。没有改动生产代码或关闭 Accordin admission。本报告中的 FlexGuard 是本轮重新测量的数据。

## 配置与计时

- 同一份 FlexGuard suite 的 LevelDB 1.20 `db_bench`，SHA-256 为 `5f4ee4e128e60af6ff13a434d82c39bfee52ff80dc10d815f5a6f60b93034171`；与此前完整 suite 相同。当前两个 Accordin DSO 与完成 Streamcluster 测量时一致。
- AArch64，96 个物理核，192 个 worker，CPU affinity 为 0–95；performance governor，记录的 CPU0 频率为 2.6 GHz。内存使用系统默认 NUMA 策略。
- `--threads=192 --time_ms=30000`，默认键空间 1,000,000、value 100 B；使用同一构建的其它默认设置，Snappy 未启用。这里是限时测试，fillrandom 的完成写入数不固定为 100 万。
- readrandom：每轮从同一单线程 fillseq 种子复制独立数据库，设置 `--use_existing_db=1`。开始前逐文件核对复制库与种子的大小和 SHA-256。种子的只读扫描审计在独立副本中数出 1,000,000 项。
- fillrandom：每轮从不存在的独立 DB 路径开始，设置 `--use_existing_db=0`。所有 DB 均位于 `/tmp` tmpfs，结果不代表物理磁盘吞吐量。完成后仅删除本 runner 创建的临时数据库。
- Accordin BPF/admission 开启，stats-only 关闭；FlexGuard 使用此前的 HYBRID_MCS + BPF + CONDVARS_BLOCK 构建。各轮旋转三种锁的顺序，全程串行并持有共用 benchmark flock。
- 吞吐量使用 `BENCH_TOTAL` 的总完成操作数 / 合并后的实际墙钟区间；不使用各线程累计 `micros/op` 的倒数。无 perf 采样开销。

```sh
db_bench --threads=192 --time_ms=30000 --benchmarks=readrandom --use_existing_db=1 --db=<seed-copy>
db_bench --threads=192 --time_ms=30000 --benchmarks=fillrandom --use_existing_db=0 --db=<fresh-db>
```

## 结果

单位为 Kops/s，即每秒 1,000 次操作；越高越好。倍数使用同工作负载本轮 Accordin 均值 / FlexGuard 均值。

| 工作负载 | 锁 | 三次 Kops/s | 均值 Kops/s | CV | 相对 FlexGuard |
|---|---|---|---:|---:|---:|
| readrandom | mcs_accordin | 572.793 / 609.776 / 580.579 | 587.716 | 3.32% | 1.495× |
| readrandom | mcs_tas_accordin | 924.510 / 957.316 / 953.849 | 945.225 | 1.91% | 2.405× |
| readrandom | flexguard | 399.571 / 389.800 / 389.825 | 393.066 | 1.43% | 1.000× |
| fillrandom | mcs_accordin | 55.062 / 54.732 / 52.778 | 54.191 | 2.28% | 1.465× |
| fillrandom | mcs_tas_accordin | 59.118 / 55.898 / 59.365 | 58.127 | 3.33% | 1.572× |
| fillrandom | flexguard | 37.750 / 36.391 / 36.814 | 36.985 | 1.88% | 1.000× |

三次重复用于观察本机该配置下的吞吐量与波动，不将小幅差异当作统计显著性证明。

## 与方案 1+3 之前的记录对照

下表的旧值取自[此前完整 suite](../flexguard-suite-20260905/README.md)，不是本轮重跑旧版 Accordin。当前与 FlexGuard 的直接比较应使用上表。

| 工作负载 | 锁 | 旧均值 Kops/s | 新均值 Kops/s | 新 / 旧 |
|---|---|---:|---:|---:|
| readrandom | mcs_accordin | 611.338 | 587.716 | 0.961× |
| readrandom | mcs_tas_accordin | 948.733 | 945.225 | 0.996× |
| readrandom | flexguard | 383.314 | 393.066 | 1.025× |
| fillrandom | mcs_accordin | 16.666 | 54.191 | 3.252× |
| fillrandom | mcs_tas_accordin | 16.818 | 58.127 | 3.456× |
| fillrandom | flexguard | 38.908 | 36.985 | 0.951× |

源码中 `DBImpl::Get` 主要在获取/释放版本引用前后重获普通 mutex；`DBImpl::Write` 则让各 writer 在自己的 condvar 上等待，并在批次完成时逐个 signal 完成者及下一个队首。这解释了为什么两类负载可能对 condvar 改动有不同敏感度；本轮没有做逐项消融，不能把全部变化精确归因于某一条原子操作或某一个队列。

## 验证与数据

18 次运行均正常退出、无超时，实际进程线程数至少 193。每次保留实际库映射和 BPF fd；Accordin 的 sched_ext enable_seq 增加一次，退出后恢复 disabled。FlexGuard 使用跟踪 BPF，不启用 sched_ext，enable_seq 保持不变。结束时重新核对可执行文件、DSO、相关源码和种子内容，哈希均未改变。

这次是限时读写，完成操作数随实现而不同，因此不要求各轮数据库输出哈希相同。种子扫描、退出状态及错误日志检查不等价于完整的 LevelDB 数据一致性验证。

- [汇总](summary.json)、[逐轮数据](results.csv)、[库映射/BPF/命令记录](results.jsonl)。
- [配置、种子清单与哈希](metadata.json)、[运行日志归档](logs.tar.gz)、[种子扫描](seed-audit.log)。
- [runner](run.py)、[报告生成脚本](report.py)。依赖前述 suite 中已构建的 `db_bench`、FlexGuard DSO 和 `/tmp/accordin-flexguard-suite-20260905/seed`；构建方法见历史 suite 报告。

在仓库根目录执行以下命令可重测，结果目录必须没有既存的 results.jsonl，防止混入历史样本：

```sh
sudo python3 docs/benchmarks/leveldb-relock-20260905/run.py --out target/leveldb-relock-repeat
```
