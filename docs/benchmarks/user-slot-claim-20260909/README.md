# 用户态名额认领：ACCORDIN_USER_CLAIM 同场 A/B

提交 `ef669b73e0277a7201a81b5b753ba4d41f692a35`（在 `e25ab5f` 之上）让走慢路径的竞争线程在
`admission.demand <= 0`（NORMAL_DSQ 与 bank 都没有排队者）时于用户态续用或认领本 CPU 的
admission 名额，否则仍按旧路径发布 `USER_WAITING` 并 `sched_yield()`。开关
`ACCORDIN_USER_CLAIM=0` 在同一个二进制里关掉这条快路径，因此下面每一组对照都是同一次会话、
同一份构建：arm `claim` 为默认行为，arm `yield` 为 `ACCORDIN_USER_CLAIM=0`。本报告不与其它
会话的数字比较。

## 配置

- 主机：x86_64，Intel Xeon Gold 5318Y @ 2.10 GHz，2 socket × 24 核，48 个逻辑 CPU 全部在线
  （`0-47`），2 个 NUMA 节点，无 SMT；governor 为 performance，CPU0 频率 2,100,000 kHz；
  内核 7.0.0-30-generic，HZ=1000。主机为共享机器，测量期间 `top` 快照中除被测进程外没有
  其它进程占用 CPU。
- 代码：测量开始时 HEAD 为 `ef669b7`；会话进行中另一个 agent 在其上提交了 `9fc1b0a`
  （只改 `Makefile`、`README.md`、`docs/plans/`、`scripts/`），因此部分 `metadata.json`
  记录的 commit 为 `9fc1b0a`。第 1、2、4 节的全部运行期间 `src/`、`include/`、
  `third_party/litl/src/` 无改动，各 runner 在自己会话结束时复核动态库哈希未变，被测代码
  即 `ef669b7` 的构建。第 3 节最后一次采样例外，见该节说明。
- 构建为 `make -j` 与 `make litl`。两个 arm 共用下列产物，会话内不重新构建：

| 文件 | SHA-256 |
|---|---|
| `target/release/libmcs_accordin_direct.so` | `17bf1bb7e296c5fdfa77bdda0ea5f9f372eee5ddeaa168ffad7da883f4c18024` |
| `target/release/libmcs_tas_accordin_direct.so` | `b233b181ed8d0d1ef63696abc7a1009667593e1fc4c0de61c0d1a39c17413098` |
| `third_party/litl/lib/libmcsaccordin_original.so` | `73271659890984fcf3db539802ed50052fa0634e5c79373a92a7c46a30356beb` |
| `third_party/litl/lib/libmcstasaccordin_original.so` | `0fa1c9a3174195566abf1b82d65031cf0682ae3415982b6c763efb5b27097809` |
| `bench/mutexbench/mutex_bench` | `ed4a3e09df7214bd432c0b740aec1c383491d1f2defbe052cf096ca9020dc04c` |
| `db_bench`（LevelDB 1.20，`accordin-m0` 下的 flexguard suite 构建） | `f958f932acbffe73bba697e3e19898141d78c6486f06dc9830896b171b81a96d` |
| `streamcluster` | `6be5114974d37f471e9c43659381d2b0b84faaa37d75030f62bae1197184ce14` |

- 环境固定为 `MCS_*_DIRECT_DISABLE_BPF=0`、`MCS_*_DIRECT_STATS_ONLY=0`、
  `ACCORDIN_DISABLE_ADMISSION=0`、`ACCORDIN_HOOK_STATS=0`、`OMP_PROC_BIND=false`、
  `OMP_WAIT_POLICY=PASSIVE`；两个 arm 之间只差 `ACCORDIN_USER_CLAIM`。
- 吞吐量运行不开 `ACCORDIN_CV_COUNTERS`，因为计数器是热路径上的原子加。`[accordin_claim]`
  行来自每个配置单独一次开启计数器的运行，这些运行不并入吞吐量均值。
- 每次加载调度器前等待 `/sys/kernel/sched_ext/state` 为 `disabled`，全程串行并持有
  `/tmp/mutexbench-sweep-multi-lock.lock`。每次运行核对实际加载的库映射、BPF fd、
  `enable_seq` 加一、进程线程数，并比对运行前后新增的 `dmesg` 行。

## 1 低竞争下的 sched_yield 次数

`bench/mutexbench` 的 `mutex_bench`，`--lock-kind mutex`，由 LiTL 适配器
`libmcsaccordin_original.so` 接管 pthread mutex（后端 `mcs_accordin`）。每次运行
`--duration-ms 5000 --warmup-duration-ms 1000`，两个 arm 交替，每个配置每个 arm 三次。
`sched_yield` 次数由 `perf stat -e syscalls:sys_enter_sched_yield` 统计；`LD_PRELOAD` 只加在
被测进程上，不加在 `perf` 上，否则 `perf` 自己也会加载调度器。

配置命名为 `线程数-临界区纳秒-临界区外纳秒`：`low-t2-c100-o0`、`low-t4-c100-o0` 是两个线程数
下的短临界区背靠背加锁；`low-t4-c100-o3000` 在临界区外加了工作；`overload-t96-c100-o3000`
用于确认过载时快路径关闭。

| 配置 | arm | 三次 sched_yield 次数 | 均值 | claim/yield | 三次 Kops/s | 均值 Kops/s | claim/yield |
|---|---|---|---:|---:|---|---:|---:|
| low-t2-c100-o0 | claim | 74,615 / 9,778 / 9,357 | 31,250 | 0.022 | 3789.9 / 3442.3 / 3437.4 | 3556.6 | 0.562 |
| low-t2-c100-o0 | yield | 1,394,721 / 1,421,994 / 1,411,641 | 1,409,452 | — | 6328.3 / 6341.0 / 6329.4 | 6332.9 | — |
| low-t4-c100-o0 | claim | 47,916 / 57,430 / 69,302 | 58,216 | 0.014 | 4002.3 / 3960.4 / 4021.4 | 3994.7 | 0.708 |
| low-t4-c100-o0 | yield | 4,138,589 / 4,306,574 / 4,322,812 | 4,255,992 | — | 5595.5 / 5632.4 / 5689.5 | 5639.1 | — |
| low-t4-c100-o3000 | claim | 2,316 / 1,067 / 1,022 | 1,468 | 0.025 | 1054.9 / 1077.0 / 1077.1 | 1069.7 | 1.010 |
| low-t4-c100-o3000 | yield | 6,180 / 67,488 / 103,126 | 58,931 | — | 1075.7 / 1057.5 / 1045.5 | 1059.6 | — |
| overload-t96-c100-o3000 | claim | 15,626,146 / 15,583,720 / 15,589,816 | 15,599,894 | 0.993 | 2589.9 / 2580.9 / 2583.2 | 2584.7 | 0.993 |
| overload-t96-c100-o3000 | yield | 15,661,783 / 15,766,290 / 15,684,110 | 15,704,061 | — | 2595.7 / 2612.2 / 2599.3 | 2602.4 | — |

两个 arm 的 sched_yield 次数相差一到两个数量级，但同一 arm 内部也有大幅波动：
`low-t2-c100-o0` 的 claim arm 首轮 74,615 次，后两轮约 9,400 次；`low-t4-c100-o3000` 的
yield arm 三轮分别是 6,180、67,488、103,126。逐轮值全部列出。

`ACCORDIN_CV_COUNTERS=1` 的单独运行给出的计数行：

| 配置 | arm | renews | claims | undone | queued | adopted | swept | slots_left | demand |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| low-t2-c100-o0 | claim | 18,256,907 | 1,933 | 0 | 13,699 | 15,857 | 1 | 0 | 0 |
| low-t2-c100-o0 | yield | 0 | 0 | 0 | 1,392,904 | 0 | 0 | 0 | 0 |
| low-t4-c100-o0 | claim | 23,230,089 | 2,192 | 0 | 65,959 | 45,433 | 2 | 0 | 0 |
| low-t4-c100-o0 | yield | 0 | 0 | 0 | 4,181,999 | 0 | 0 | 0 | 0 |
| low-t4-c100-o3000 | claim | 20,362 | 4,227 | 0 | 746 | 4,371 | 1 | 0 | 0 |
| low-t4-c100-o3000 | yield | 0 | 0 | 0 | 29,916 | 0 | 0 | 0 | 0 |
| overload-t96-c100-o3000 | claim | 0 | 0 | 0 | 15,600,356 | 0 | 0 | 0 | 0 |
| overload-t96-c100-o3000 | yield | 0 | 0 | 0 | 15,633,416 | 0 | 0 | 0 | 0 |

三个低线程配置里 `renews + claims` 远大于 `queued`；96 线程配置下 `renews` 与 `claims` 都是 0，
两个 arm 的 `queued` 与 `sched_yield` 次数在 1% 以内。所有运行的 `undone` 为 0，
`slots_left` 为 0。

## 2 LevelDB readrandom / fillrandom，192 线程

`db_bench --threads=192 --time_ms=30000`，键空间 1,000,000、value 100 B，数据库位于 `/tmp`
tmpfs（因此不代表物理磁盘吞吐量），未启用 Snappy。readrandom 每轮从同一份单线程 fillseq 种子
（`/tmp/accordin-flexguard-suite-20260905/seed`）复制独立副本并逐文件核对大小与 SHA-256，
`--use_existing_db=1`；fillrandom 每轮使用全新路径，`--use_existing_db=0`。会话开始时对种子做
一次只读扫描审计，数出 1,000,000 项。吞吐量取 `BENCH_TOTAL` 的总完成操作数除以合并后的实际
墙钟区间，不取逐线程 `micros/op` 的倒数。同一后端内两个 arm 相邻运行，轮次之间轮转起始 cell。

| 工作负载 | 后端 | arm | 三次 Kops/s | 均值 Kops/s | CV | claim/yield |
|---|---|---|---|---:|---:|---:|
| readrandom | mcs_accordin | yield | 1195.867 / 1213.821 / 1175.360 | 1195.016 | 1.61% | — |
| readrandom | mcs_accordin | claim | 1148.181 / 1190.222 / 1203.996 | 1180.800 | 2.46% | 0.988 |
| readrandom | mcs_tas_accordin | yield | 1096.155 / 1102.730 / 1105.876 | 1101.587 | 0.45% | — |
| readrandom | mcs_tas_accordin | claim | 1025.777 / 1078.819 / 1091.776 | 1065.457 | 3.28% | 0.967 |
| fillrandom | mcs_accordin | yield | 266.619 / 260.023 / 262.899 | 263.180 | 1.26% | — |
| fillrandom | mcs_accordin | claim | 267.603 / 249.860 / 251.356 | 256.273 | 3.84% | 0.974 |
| fillrandom | mcs_tas_accordin | yield | 264.484 / 247.602 / 272.578 | 261.554 | 4.87% | — |
| fillrandom | mcs_tas_accordin | claim | 267.337 / 244.688 / 264.866 | 258.964 | 4.80% | 0.990 |

24 次运行全部有效，没有超时、非零退出、映射不符或 watchdog 弹出。每个单元格的 claim/yield
差值都小于该单元格自身逐轮值的跨度，三次重复不足以把这些差值与波动区分开。

各 arm 各后端各工作负载另做一次 `ACCORDIN_CV_COUNTERS=1` 的运行：

| 工作负载 | 后端 | arm | renews | claims | undone | queued | adopted | swept | slots_left | demand | Kops/s |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| readrandom | mcs_accordin | claim | 2 | 2 | 0 | 83,723,366 | 3 | 0 | 0 | 0 | 1202.928 |
| readrandom | mcs_accordin | yield | 0 | 0 | 0 | 84,392,799 | 0 | 0 | 0 | 0 | 1212.638 |
| readrandom | mcs_tas_accordin | claim | 8 | 3 | 0 | 66,114,688 | 11 | 0 | 0 | 0 | 1102.468 |
| readrandom | mcs_tas_accordin | yield | 0 | 0 | 0 | 61,831,614 | 0 | 0 | 0 | 0 | 1086.368 |
| fillrandom | mcs_accordin | claim | 27,086 | 2,206 | 0 | 2,049,805 | 29,291 | 0 | 0 | 0 | 272.466 |
| fillrandom | mcs_accordin | yield | 0 | 0 | 0 | 2,058,836 | 0 | 0 | 0 | 0 | 273.624 |
| fillrandom | mcs_tas_accordin | claim | 15,306 | 2,114 | 0 | 1,512,773 | 17,419 | 0 | 0 | 0 | 258.009 |
| fillrandom | mcs_tas_accordin | yield | 0 | 0 | 0 | 1,481,511 | 0 | 0 | 0 | 0 | 271.756 |

192 个线程跑在 48 个 CPU 上时 `demand > 0` 几乎总成立：readrandom 的 claim arm 在 6,600 万到
8,400 万次慢路径中只走了 4 次与 11 次快路径；fillrandom 按 `(renews + claims) / queued` 计为
1.2%（mcs_tas）到 1.4%（mcs_accordin）。计数器运行自身带原子加开销，其 Kops/s 不与上表的
吞吐量均值合并。

## 3 普通任务等待时间

在每个 arm 各一次 readrandom 运行（后端 `mcs_accordin`）的稳定段内，用 bpftrace 在
`scx_bpf_dsq_insert` 上挂 kprobe，过滤 `dsq_id == NORMAL_DSQ`（`0x100`），按 tid 记下插入
时刻，并由随后第一个把该 tid 换上 CPU 的 `sched:sched_switch` 结算。运行开始后等待 5 秒再附加。
直方图为 0–2000 us、10 us 一格的线性直方图，p50/p99 取所落桶的上界，超过 2 ms 的样本单独计数。

kprobe 能够挂上并命中，但按 db_bench 的 tgid 过滤后，每次 15–20 秒的采样窗口内只有个位数个
插入：db_bench 的 192 个 worker 几乎全程留在本地队列或 admission 队列上，很少经过 NORMAL_DSQ。
第二次采样加了不带 tgid 过滤的全机插入计数，确认同一窗口内全机有约 4 万次 NORMAL_DSQ 插入，
所以少样本来自过滤条件而不是探针失效。

`ef669b7` 构建上的两次采样（`data/normal-dsq-latency-firstpass`、`data/normal-dsq-latency-pass2`）：

| 采样 | 窗口 | arm | db_bench 插入 | 样本数 | p50 (us) | p99 (us) | max (us) | >2ms | 全机插入 | 该轮 Kops/s |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| firstpass | 15 s | claim | 2 | 2 | 10 | 20 | 12 | 0 | 未记录 | 1195.425 |
| firstpass | 15 s | yield | 3 | 3 | 20 | 20 | 15 | 0 | 未记录 | 1203.281 |
| pass2 | 20 s | claim | 2 | 2 | 20 | 20 | 12 | 0 | 41,826 | 1185.026 |
| pass2 | 20 s | yield | 1 | 1 | 20 | 20 | 12 | 0 | 36,755 | 1186.745 |

九个样本全部落在 20 us 以内，两个 arm 都没有出现 tick 长度（1 ms）的等待；但样本数只有个位数，
不足以给出有意义的分位数比较。

之后又做了一次同时记录全机直方图的采样（`data/normal-dsq-latency`）。这次运行开始前六秒，
另一个 agent 重建了 `libmcs_accordin_direct.so`（工作区里未提交的 `admission_state`
cache line 填充改动，SHA-256 变为
`15883f353c8be2ebafd2987839413f735bed455de239331b67569fac2645af31`），因此这组数字属于那个
构建，不能计入 `ef669b7` 的对照，仅作为全机口径下 NORMAL_DSQ 等待分布的参考：

| 范围 | arm | 样本数 | p50 (us) | p99 (us) | max (us) | 均值 (us) | >2ms | 插入次数 |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| db_bench 进程 | claim | 8 | 10 | 30 | 25 | 12 | 0 | 8 |
| db_bench 进程 | yield | 4 | 10 | 30 | 25 | 12 | 0 | 4 |
| 全机 | claim | 48,215 | 20 | 70 | 12,043 | 15 | 12 | 48,350 |
| 全机 | yield | 42,425 | 10 | 60 | 17,985 | 13 | 7 | 43,151 |

## 4 streamcluster，192 线程

`streamcluster 10 30 512 32768 32768 2000 none <output> 192`，两个后端各 arm 一次。四次运行的
聚类输出 SHA-256 均为 `13b33997906352993b3d67c0d86cf2a984741f96163597990e935d3f72cc5b84`
（本机 x86_64 参考值）。下表的比较用 runner 记录的墙钟秒；程序内部的 "Benchmark time" 计数
依赖 `tsc_khz`，一并列出但不作为比较依据。单位秒，越低越快。

| 后端 | arm | 墙钟秒 | claim/yield | 程序内计时秒 |
|---|---|---:|---:|---:|
| mcs_accordin | claim | 38.557 | 0.975 | 38.264 |
| mcs_accordin | yield | 39.558 | — | 39.267 |
| mcs_tas_accordin | claim | 37.461 | 0.982 | 37.195 |
| mcs_tas_accordin | yield | 38.165 | — | 37.926 |

每个 arm 只有一次运行，不给出波动估计。

## 结论

- 低线程数下快路径按设计生效：2 线程与 4 线程的短临界区配置里 `sched_yield` 次数降到
  yield arm 的 1.4%–2.5%，计数行显示慢路径主要由 `renews` 承担。
- 96 线程过载配置下 `renews = claims = 0`，两个 arm 的 `sched_yield` 次数与吞吐量相差 0.7%，
  快路径确实关闭。
- 临界区外有工作的 4 线程配置（`low-t4-c100-o3000`）吞吐量 claim/yield 为 1.010；两个背靠背
  加锁配置（`o0`）的 claim arm 吞吐量分别为 yield arm 的 0.562 与 0.708。
- LevelDB 192 线程下 `demand > 0` 几乎总成立，快路径在 readrandom 的一轮里只走了 4 次与 11 次，
  在 fillrandom 中占慢路径的 1.2%–1.4%；四个单元格的 claim/yield 在 0.967 到 0.990 之间，
  差值小于各自逐轮值的跨度。
- readrandom 期间 db_bench 自己几乎不进 NORMAL_DSQ（15–20 秒窗口内个位数次插入，同期全机
  约 4 万次），`ef669b7` 上取到的九个样本全部在 20 us 以内，两个 arm 都没有 tick 长度的等待；
  样本太少，不足以做分位数比较。
- streamcluster 四次运行输出哈希一致，claim arm 的墙钟时间为 yield arm 的 0.975 与 0.982。
- 所有运行 `undone = 0`、`slots_left = 0`、`demand = 0`（卸载时读取）；`dmesg` 中只有调度器
  enable/disable 行，没有 watchdog 弹出，没有无效或被丢弃的运行。

## 数据与脚本

- [mutexbench 逐轮数据](data/mutexbench/results.csv)、[JSONL](data/mutexbench/results.jsonl)、
  [汇总](data/mutexbench/summary.json)、[配置](data/mutexbench/metadata.json)
- [LevelDB 吞吐量](data/leveldb/results.csv)、[JSONL](data/leveldb/results.jsonl)、
  [汇总](data/leveldb/summary.json)、[表格](data/leveldb/summary.md)、
  [配置](data/leveldb/metadata.json)
- [LevelDB 计数器运行](data/leveldb-counters/results.csv)、
  [JSONL](data/leveldb-counters/results.jsonl)、[汇总](data/leveldb-counters/summary.json)
- NORMAL_DSQ 延迟，`ef669b7` 构建：[firstpass](data/normal-dsq-latency-firstpass/summary.json)、
  [pass2](data/normal-dsq-latency-pass2/summary.json)；后续在改动过的构建上采到的全机直方图：
  [results.jsonl](data/normal-dsq-latency/results.jsonl)、
  [汇总](data/normal-dsq-latency/summary.json)
- [streamcluster](data/streamcluster/results.csv)、[JSONL](data/streamcluster/results.jsonl)、
  [汇总](data/streamcluster/summary.json)
- [运行日志、perf 输出、bpftrace 输出与聚类输出归档](logs.tar.gz)
- runner：[mutexbench_yield.py](mutexbench_yield.py)、[leveldb.py](leveldb.py)、
  [normal_dsq_latency.py](normal_dsq_latency.py) 与 [normal_dsq_latency.bt](normal_dsq_latency.bt)、
  [streamcluster.py](streamcluster.py)；表格由 [report.py](report.py) 生成。
  运行机制、有效性判定与 sched_ext 检查复用
  [cv-custody 的 runner](../cv-custody-20260906/run.py) 与
  [其 streamcluster runner](../cv-custody-20260906/streamcluster.py)，这里只替换了 cell 的构造：
  两个 arm 是同一份构建的两个环境变量取值，不各自检出 worktree。

在仓库根目录重测（每个脚本自己获取共用 flock，不要再套一层 `flock`）：

```sh
sudo python3 docs/benchmarks/user-slot-claim-20260909/mutexbench_yield.py --out target/user-slot-claim-repeat/mutexbench
sudo python3 docs/benchmarks/user-slot-claim-20260909/leveldb.py --out target/user-slot-claim-repeat/leveldb --repeats 3
sudo python3 docs/benchmarks/user-slot-claim-20260909/leveldb.py --out target/user-slot-claim-repeat/leveldb-counters --repeats 1 --counters
sudo python3 docs/benchmarks/user-slot-claim-20260909/normal_dsq_latency.py --out target/user-slot-claim-repeat/normal-dsq-latency --backend mcs_accordin --trace-seconds 20
sudo python3 docs/benchmarks/user-slot-claim-20260909/streamcluster.py --out target/user-slot-claim-repeat/streamcluster --repeats 1 --modes stream
python3 docs/benchmarks/user-slot-claim-20260909/report.py --root target/user-slot-claim-repeat --write-csv
```
