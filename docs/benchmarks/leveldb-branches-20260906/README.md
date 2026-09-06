# fullhook-admission 与 simplify：LevelDB 192 线程对比

本轮比较两个固定提交，两个锁后端、两个工作负载各尝试三次，共 24 次运行；有效样本和超时分开记录。两边均重新构建并重新测量，不使用历史吞吐量作为本轮对照。

| 分支 | 固定提交 | pthread 入口 | condvar 默认策略 |
|---|---|---|---|
| 当前分支 fullhook-admission | `4e5998e21c458e9b162855ef3c3d5c7e42b42ebb` | `libmcs_accordin_fullhook.so` / `libmcs_tas_accordin_fullhook.so` | admission 授权下自旋，默认 1000 µs，之后 futex 停泊 |
| simplify | `8fc18994aa51f27b91bf6b490a575a309add8da3` | 标准 LiTL `libmcsaccordin_original.so` / `libmcstasaccordin_original.so`，各自链接本分支 direct 库 | 方案 1+3：预发布 relock 请求、沿用 epoch、mutex 接力唤醒 |

## 配置

- 使用同一份此前 FlexGuard suite 的 LevelDB 1.20 `db_bench`，SHA-256 为 `5f4ee4e128e60af6ff13a434d82c39bfee52ff80dc10d815f5a6f60b93034171`。本次使用其限时版本，参数与此前 LevelDB 测量一致，没有改用 fullhook 实验脚本默认的 LevelDB 1.23/固定操作数配置。
- 192 个 worker，CPU affinity 0–95（96 个物理核）；performance governor，记录的 CPU0 频率为 2.6 GHz，默认 NUMA 内存策略。
- 每次 `--time_ms=30000`，默认键空间 1,000,000、value 100 B。Snappy 未启用；数据库位于 `/tmp` tmpfs，不代表物理磁盘吞吐量。
- readrandom 每轮复制同一 fillseq 种子，使用 `--use_existing_db=1`；逐文件比较副本与种子的大小和 SHA-256。独立副本的原生 readseq 审计数出 1,000,000 项。
- fillrandom 每轮从新的空数据库路径开始，使用 `--use_existing_db=0`。这是限时测试，完成写入数不固定为 100 万。
- 两边 BPF/admission 均开启，stats-only 和 hook 统计关闭；fullhook 显式设置 `ACCORDIN_CV_SPIN_US=1000`，与该提交默认值一致。没有改动任何分支源码或导入对方优化。
- 两个 detached worktree 分别构建，LD_PRELOAD 和 LD_LIBRARY_PATH 指向对应 worktree。每次核对 `/proc/<pid>/maps` 的锁库集合与预期完全相符，避免混用 direct 库。
- 全部测试串行并持有 `/tmp/mutexbench-sweep-multi-lock.lock`，每轮旋转四个配置的顺序。吞吐量使用 BENCH_TOTAL 的总操作数 / 实际墙钟区间，无 perf 采样。

```sh
db_bench --threads=192 --time_ms=30000 --benchmarks=readrandom --use_existing_db=1 --db=<seed-copy>
db_bench --threads=192 --time_ms=30000 --benchmarks=fillrandom --use_existing_db=0 --db=<fresh-db>
```

## 结果

单位 Kops/s（每秒 1,000 次操作），越高越好；“当前 / simplify”按同一种锁比较三次均值。

| 工作负载 | 锁 | simplify 均值 | 当前分支均值 | 当前 / simplify | 吞吐量变化 |
|---|---|---:|---:|---:|---:|
| readrandom | mcs_accordin | 595.660 | 607.300 | 1.020× | +2.0% |
| readrandom | mcs_tas_accordin | 950.121 | 661.382 | 0.696× | -30.4% |
| fillrandom | mcs_accordin | 52.290 | —（1/3 有效） | — | — |
| fillrandom | mcs_tas_accordin | 51.816 | 145.996 | 2.818× | +181.8% |

### 逐轮与波动

| 工作负载 | 锁 | 分支 | 三次 Kops/s | CV |
|---|---|---|---|---:|
| readrandom | mcs_accordin | fullhook-admission | 609.465 / 606.483 / 605.951 | 0.31% |
| readrandom | mcs_accordin | simplify | 579.328 / 580.960 / 626.693 | 4.51% |
| readrandom | mcs_tas_accordin | fullhook-admission | 649.722 / 670.165 / 664.259 | 1.59% |
| readrandom | mcs_tas_accordin | simplify | 926.826 / 951.290 / 972.248 | 2.39% |
| fillrandom | mcs_accordin | fullhook-admission | 超时/无效 / 121.459 / 超时/无效 | — |
| fillrandom | mcs_accordin | simplify | 55.854 / 48.554 / 52.461 | 6.99% |
| fillrandom | mcs_tas_accordin | fullhook-admission | 143.323 / 148.432 / 146.234 | 1.76% |
| fillrandom | mcs_tas_accordin | simplify | 57.518 / 49.107 / 48.823 | 9.53% |

有效运行用于观察这台机器、这一配置下的差异与波动；小幅差异不视为统计显著性的证明。若某配置不足三次有效结果，不为其计算分支对比倍数，也不把超时记为零吞吐量。

## 超时记录

- `fillrandom-192-fullhook-admission-mcs_accordin-r1`：wall=120.166 s，returncode=-9，未产生有效 BENCH_TOTAL；锁库映射匹配预期，sched_ext 序号 787 → 788。
- `fillrandom-192-fullhook-admission-mcs_accordin-r3`：wall=120.158 s，returncode=-9，未产生有效 BENCH_TOTAL；锁库映射匹配预期，sched_ext 序号 805 → 806。

第三轮在进程存活 65.73 s 时记录到 194 个线程，wchan 分布为 `{'futex_wait_queue': 58, '0': 136}`。

perf 采样器超过自身 10 秒上限，数据文件不完整；没有可用的热点报告。本轮未定位超时根因，不能仅凭源码内存序差异判定原因。线程快照和采样错误日志保存在日志归档中。

## 如何理解这次比较

这是整套分支实现的对比，包含 pthread 拦截层、mutex 布局、TLS 模型、BPF 入队/dispatch/yield 策略及 condvar 行为的共同差异，不能单独归因为 condvar 自旋或接力队列。

fullhook 直接在 pthread_mutex_t 中存放原始锁，使用 initial-exec TLS；MCS 节点池为 8 项，MCS-TAS 的 tail/locked 使用紧凑布局。simplify 的 LiTL 保存内部对象指针，direct MCS 节点池为 4 项，MCS-TAS 的 tail/locked 分开对齐。MCS trylock 成功 CAS 的内存序也不同：当前分支为 acquire，simplify 为 acq_rel。本轮保留各提交原样。

LevelDB Get 主要在读取版本引用前后使用普通 mutex；Write 则使用 writer 队列和每个 writer 的条件变量。因此两类负载对上述差异的敏感度可能不同；本次没有逐项消融或 CPU 采样来分解贡献。

## 验证与复现

两个 worktree 的 make check / make check-bpf 均通过；simplify 额外通过 check-litl / check-litl-bpf，包含 C++、取消/超时及 NDEBUG 检查。有效性能运行正常退出；全部尝试均记录实际进程线程数、库映射、BPF fd 和 sched_ext enable_seq。超时记录保留返回码 -9、120 秒的墙钟上限以及原始进度日志，不计算吞吐量。超过 65 秒仍未退出的复测由独立观察脚本记录线程等待位置，并尝试两秒 perf 采样；本轮 perf 采样器自身超时，生成的数据不完整，未用于定位热点。这些诊断均发生在目标 30 秒测量期之外。结束后 sched_ext 恢复 disabled，源码、库、二进制与原种子的哈希均未改变。

这些检查用于验证构建、工作量与运行条件，不将性能运行视为两分支所有 pthread 语义等价或完整数据一致性证明。

- [配置与哈希](metadata.json)、[原始记录](results.jsonl)、[逐轮 CSV](results.csv)、[汇总](summary.json)。
- [构建 fullhook](build-fullhook.log)、[构建 simplify](build-simplify.log)、[测试日志](check.log)、[种子审计](seed-audit.log)、[性能日志归档](logs.tar.gz)。
- [runner](run.py)、[续跑脚本](resume.py)、[停滞观察脚本](watch_stalls.py) 和 [报告脚本](report.py)。首个超时使 runner 停止；续跑脚本跳过所有已记录尝试，完成剩余矩阵并保留后续失败。原始目录为 `target/leveldb-branches-20260906/`，两个独立构建的源码和 DSO 也保留于此。

复现需要相同的 LevelDB 二进制和种子，来源为此前 FlexGuard suite。从仓库根目录准备相同提交：

```sh
git worktree add --detach target/leveldb-branches-20260906/fullhook-src 4e5998e21c458e9b162855ef3c3d5c7e42b42ebb
git worktree add --detach target/leveldb-branches-20260906/simplify-src 8fc18994aa51f27b91bf6b490a575a309add8da3
make -C target/leveldb-branches-20260906/fullhook-src -j8 all
make -C target/leveldb-branches-20260906/simplify-src -j8 litl
sudo python3 docs/benchmarks/leveldb-branches-20260906/run.py --out target/leveldb-branches-repeat
```

若 worktree 已存在，复用已构建的对应提交即可。结果目录必须没有既存 results.jsonl；runner 不覆盖旧样本，遇到首个无效运行会停止。本轮 resume.py 固定读取本轮 target 目录，记录所有剩余尝试，未重新执行已失败的轮次。
