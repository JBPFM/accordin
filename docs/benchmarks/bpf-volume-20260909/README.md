# BPF 文本量：`fbc463e` 与 `432bee6` 同场 A/B

`be0dd22`、`2a872ce`、`432bee6` 三个提交把调度器的 BPF 程序压小，但不改调度决策：
`ops.dispatch` 里四份内联的 admission 扫描和两份队头读取变成子程序，只在运行时关掉的 flush
分支连同其环境变量一起删掉，加载前发布一次的配置改成 `const volatile` 交给验证器折叠。三者
合起来让内核实际加载的文本从 2765 条降到 2162 条（本报告实测，见下），同时 `ops.dispatch`
的验证器处理指令数从 14,976 涨到 42,670——静态子程序在每个调用点都要重新展开一次。

项目此前的结论是：几百条加载文本会让 LevelDB readrandom 掉 10–15%。本次 A/B 要回答的就是
这条敏感性跟的是「加载文本」还是「验证器处理量」这两个反向移动的量中的哪一个，以及
mutexbench、LevelDB readrandom/fillrandom、streamcluster 各自是持平、变好还是变差。

arm `bpfbase` 为 `fbc463e`（三次改动之前），arm `bpftip` 为 `432bee6`。所有比值都是
tip/base。本报告不与其它会话的数字比较。

## 配置

- 主机：x86_64，Intel Xeon Gold 5318Y @ 2.10 GHz，2 socket × 24 核，48 个逻辑 CPU 全部在线
  （`0-47`），2 个 NUMA 节点，无 SMT；governor 为 performance；内核
  `7.0.0-30-generic #30-Ubuntu SMP PREEMPT_DYNAMIC`。主机为共享机器。
- 代码：仓库 `/mnt/data/home/jz/accordin-simplify`，分支 `cv_admission`，测量期间 HEAD 为
  `432bee6`，工作区只有 `bench/mutexbench` 子模块指针的本地改动（`multilockbench` 二进制），
  与被测代码无关。两个 arm 各自在 `target/cv-custody-20260906/{bpfbase,bpftip}-src` 的独立
  detached worktree 里检出并构建（`make -j all` 与 `make litl`），会话内不重新构建，
  runner 在会话结束时复核源文件与动态库哈希未变、worktree 未变脏。
- 编译器：`clang 21.1.8`（BPF 与直连库），`cc 14.3.0`（LiTL 适配器）。
- 每个 arm 的产物：

| 文件 | bpfbase (`fbc463e`) | bpftip (`432bee6`) |
|---|---|---|
| `target/release/libmcs_accordin_direct.so` | `4915e8b7238ae6978f7808b1a576b69a3ba5b8705f2d21ea2739abe2ffca0b54` | `b9fb8e7d8514c0c250d704888ba926355c585f029b93ac70484d36a5bb837f35` |
| `target/release/libmcs_tas_accordin_direct.so` | `de97693d664b711351f3674663ce3eed998b0bf66068ac493a876673c6def892` | `e3e228957250733617fb3eecc1f1c3bae9b9731e2cf33eea8ac3befbfe83c152` |
| `third_party/litl/lib/libmcsaccordin_original.so` | `553e471c13758b94624b8cd95b952d28c29fe3a350e01075d2639057f4a271c8` | `abf19a8f0afd13b89d5c54ef6b90a02e643c7051442a62aaaa0ee372005c4448` |
| `third_party/litl/lib/libmcstasaccordin_original.so` | `ee340c141a64be38506d5294ea7161c29cdf70bd8e6e1cf2f9fa35e158a4882a` | `4c86b57fb87f39ae17019378cb1781de6271f86aa2c409301691a565c94a875c` |

- 两个 arm 共用的被测程序：

| 文件 | SHA-256 |
|---|---|
| `bench/mutexbench/mutex_bench`（子模块 `b314b6d`） | `ed4a3e09df7214bd432c0b740aec1c383491d1f2defbe052cf096ca9020dc04c` |
| `db_bench`（LevelDB 1.20，`accordin-m0` 下的 flexguard suite 构建） | `f958f932acbffe73bba697e3e19898141d78c6486f06dc9830896b171b81a96d` |
| `streamcluster` | `6be5114974d37f471e9c43659381d2b0b84faaa37d75030f62bae1197184ce14` |

- 环境固定为 `MCS_*_DIRECT_DISABLE_BPF=0`、`MCS_*_DIRECT_STATS_ONLY=0`、
  `ACCORDIN_DISABLE_ADMISSION=0`、`ACCORDIN_HOOK_STATS=0`、`OMP_PROC_BIND=false`、
  `OMP_WAIT_POLICY=PASSIVE`。两个 arm 之间只差代码。
- **`ACCORDIN_CV_COUNTERS=0` 加在两个 arm 上**。cv-custody runner 的 `CONFIG` 默认写
  `ACCORDIN_CV_COUNTERS=1`，而 `432bee6` 把计数器的开关做成加载期常量：开着计数器等于把
  诊断用的原子加编译回热路径，测的就不是出厂配置。per-arm 环境覆盖在 runner 里排在 `CONFIG`
  之后，因此 `0` 生效；有效性判定不要求日志里出现 `[accordin_cv]` 行，60 次运行全部有效。
  代价是本报告没有计数器表。
- 每次加载调度器前等待 `/sys/kernel/sched_ext/state` 为 `disabled`；三个 runner 依次串行
  执行，全程持有 `/tmp/mutexbench-sweep-multi-lock.lock`，任何时刻只有一次调度器加载。
  每次运行核对实际加载的库映射、BPF fd、`enable_seq` 加一、进程线程数，并比对运行前后新增的
  `dmesg` 行。
- runner 未改动：LevelDB 与 streamcluster 直接用 `cv-custody-20260906` 的两个脚本；
  mutexbench 的 [mutexbench.py](mutexbench.py) 是本目录新写的，arm 是提交而不是环境变量，
  worktree 构建、库选择、环境清洗、串行化与 sched_ext 检查都从 `cv-custody-20260906/run.py`
  取用，只有 `mutex_bench` 二进制取自主 worktree（子模块不随 worktree 检出）。适配器仍取自
  arm 自己的 worktree，直连库经 `LD_LIBRARY_PATH` 解析到该 arm 的 `target/release`，每次运行
  用 `/proc/<pid>/maps` 核对两个库路径确实是该 arm 的。

## 1 指令数

两张表都是本次会话实测，不是从提交说明抄的。

「加载」一列是内核里 `bpftool prog show` 报出的 xlated 指令数与 JIT 字节数，采样时机是一个
两线程 `mutex_bench` 把调度器挂上之后；子程序文本计入其主程序。「验证器处理」一列来自
`ACCORDIN_VERIFY_ONLY=1`，它给每个程序开验证器日志加载一次、读出 `processed N insns` 后立即
销毁，不 attach。两次测量都在同一把锁下、`ACCORDIN_CV_COUNTERS=0` 下进行。

| 程序 | base 加载指令 | tip 加载指令 | tip/base | base JIT 字节 | tip JIT 字节 |
|---|---:|---:|---:|---:|---:|
| dispatch | 733 | 384 | 0.524 | 3,212 | 1,741 |
| cv_flush | 423 | 340 | 0.804 | 1,901 | 1,526 |
| init | 337 | 326 | 0.967 | 1,559 | 1,527 |
| enqueue | 327 | 281 | 0.859 | 1,507 | 1,315 |
| dump | 249 | 206 | 0.827 | 1,109 | 929 |
| stopping | 182 | 144 | 0.791 | 822 | 697 |
| tick | 158 | 146 | 0.924 | 726 | 707 |
| exit | 118 | 113 | 0.958 | 576 | 552 |
| exit_task | 102 | 92 | 0.902 | 501 | 448 |
| yield | 78 | 72 | 0.923 | 372 | 344 |
| select_cpu | 42 | 42 | 1.000 | 201 | 201 |
| runnable | 8 | 8 | 1.000 | 50 | 50 |
| quiescent | 8 | 8 | 1.000 | 48 | 48 |
| 合计 | 2,765 | 2,162 | 0.782 | 12,584 | 10,085 |

| 程序 | base 验证器处理 | tip 验证器处理 | tip/base |
|---|---:|---:|---:|
| dispatch | 14,976 | 42,670 | 2.849 |
| cv_flush | 3,311 | 3,221 | 0.973 |
| init | 3,056 | 3,021 | 0.989 |
| enqueue | 591 | 484 | 0.819 |
| dump | 477 | 387 | 0.811 |
| stopping | 362 | 349 | 0.964 |
| tick | 319 | 314 | 0.984 |
| exit_task | 317 | 323 | 1.019 |
| exit | 305 | 313 | 1.026 |
| yield | 88 | 77 | 0.875 |
| select_cpu | 50 | 49 | 0.980 |
| runnable | 7 | 7 | 1.000 |
| quiescent | 7 | 7 | 1.000 |
| 合计 | 23,866 | 51,222 | 2.146 |

两个量方向相反：加载文本降 21.8%（JIT 字节降 19.9%），验证器处理量涨 1.15 倍，其中
`ops.dispatch` 一项就从 14,976 涨到 42,670。提交说明里另有一套按 ELF 文本计的数字
（全部程序 3042 → 2557 → 2442 → 2478），口径与本表不同：ELF 文本按每个调用点重复计入子程序，
`432bee6` 的计数器门控还让 ELF 文本反涨 36 条。

## 2 mutexbench，低竞争与过载

`bench/mutexbench` 的 `mutex_bench`，`--lock-kind mutex`，由 LiTL 适配器
`libmcsaccordin_original.so` 接管 pthread mutex（后端 `mcs_accordin`）。每次运行
`--duration-ms 5000 --warmup-duration-ms 1000`，两个 arm 交替，每个配置每个 arm 三次，
不挂 perf。配置命名为 `线程数-临界区纳秒-临界区外纳秒`。

| 配置 | arm | 三次 Kops/s | 均值 Kops/s | CV | tip/base |
|---|---|---|---:|---:|---:|
| low-t2-c100-o0 | base | 3359.3 / 3563.4 / 3569.9 | 3497.5 | 3.42% | — |
| low-t2-c100-o0 | tip | 3410.2 / 3290.9 / 3549.5 | 3416.9 | 3.79% | 0.977 |
| low-t4-c100-o0 | base | 4011.5 / 3994.0 / 4019.1 | 4008.2 | 0.32% | — |
| low-t4-c100-o0 | tip | 4017.0 / 4046.9 / 4099.7 | 4054.5 | 1.03% | 1.012 |
| low-t4-c100-o3000 | base | 1075.2 / 1072.4 / 1070.8 | 1072.8 | 0.21% | — |
| low-t4-c100-o3000 | tip | 1071.7 / 1071.7 / 1073.9 | 1072.4 | 0.12% | 1.000 |
| overload-t96-c100-o3000 | base | 2604.5 / 2602.0 / 2586.3 | 2597.6 | 0.38% | — |
| overload-t96-c100-o3000 | tip | 2607.7 / 2622.4 / 2621.0 | 2617.0 | 0.31% | 1.007 |

四个配置的差值都落在同一 arm 三次运行自身的跨度里：`low-t2-c100-o0` 两个 arm 的 CV 都接近
3.5%，2.3% 的差值区分不开；另外三个配置的差值为 0.0%–1.2%，而 CV 在 0.1%–1.0%。

## 3 LevelDB readrandom / fillrandom，192 线程

`db_bench --threads=192 --time_ms=30000`，键空间 1,000,000、value 100 B，数据库位于 `/tmp`
tmpfs（因此不代表物理磁盘吞吐量），未启用 Snappy。readrandom 每轮从同一份单线程 fillseq 种子
（`/tmp/accordin-flexguard-suite-20260905/seed`）复制独立副本并逐文件核对大小与 SHA-256，
`--use_existing_db=1`；fillrandom 每轮使用全新路径，`--use_existing_db=0`。会话开始时对种子做
一次只读扫描审计，数出 1,000,000 项。吞吐量取 `BENCH_TOTAL` 的总完成操作数除以合并后的实际
墙钟区间。同一后端内两个 arm 相邻运行，轮次之间轮转起始 cell。

| 工作负载 | 后端 | arm | 三次 Kops/s | 均值 Kops/s | CV | tip/base |
|---|---|---|---|---:|---:|---:|
| readrandom | mcs_accordin | base | 1329.530 / 1337.515 / 1349.346 | 1338.797 | 0.74% | — |
| readrandom | mcs_accordin | tip | 1217.800 / 1211.878 / 1204.897 | 1211.525 | 0.53% | 0.905 |
| readrandom | mcs_tas_accordin | base | 1142.346 / 1185.560 / 1158.568 | 1162.158 | 1.88% | — |
| readrandom | mcs_tas_accordin | tip | 1099.837 / 1116.741 / 1088.267 | 1101.615 | 1.30% | 0.948 |
| fillrandom | mcs_accordin | base | 271.000 / 268.455 / 263.403 | 267.619 | 1.45% | — |
| fillrandom | mcs_accordin | tip | 272.827 / 272.121 / 280.900 | 275.283 | 1.77% | 1.029 |
| fillrandom | mcs_tas_accordin | base | 272.498 / 271.223 / 251.367 | 265.029 | 4.47% | — |
| fillrandom | mcs_tas_accordin | tip | 271.869 / 272.087 / 286.002 | 276.653 | 2.93% | 1.044 |

readrandom 两个后端的逐轮值完全不重叠：`mcs_accordin` 的 base 最低 1329.530 仍高于 tip 最高
1217.800，`mcs_tas_accordin` 的 base 最低 1142.346 高于 tip 最高 1116.741。fillrandom 方向相反，
`mcs_accordin` 的逐轮值也不重叠（base 最高 271.000，tip 最低 272.121）；`mcs_tas_accordin`
的两个 arm 在边缘相接（base 最高 272.498，tip 最低 271.869），其 4.44% 的差值与 base 自身
4.47% 的 CV 同量级。

## 4 streamcluster，192 线程

`streamcluster 10 30 512 32768 32768 2000 none <output> 192`，两个后端各 arm 三次，
arm 交替。12 次运行的聚类输出 SHA-256 全部为
`13b33997906352993b3d67c0d86cf2a984741f96163597990e935d3f72cc5b84`（本机 x86_64 参考值）。
单位秒，越低越快。

| 后端 | arm | 三次秒 | 均值秒 | CV | tip/base |
|---|---|---|---:|---:|---:|
| mcs_accordin | base | 36.791 / 35.145 / 36.769 | 36.235 | 2.61% | — |
| mcs_accordin | tip | 35.063 / 35.415 / 35.111 | 35.196 | 0.54% | 0.971 |
| mcs_tas_accordin | base | 36.254 / 36.334 / 36.787 | 36.458 | 0.79% | — |
| mcs_tas_accordin | tip | 34.477 / 35.779 / 35.566 | 35.274 | 1.98% | 0.968 |

`mcs_tas_accordin` 的逐轮值不重叠（base 最低 36.254 高于 tip 最高 35.779）；`mcs_accordin`
的 base 有一轮 35.145 落进 tip 的区间，2.9% 的差值与 base 自身 2.61% 的 CV 同量级。

runner 记录的是每次运行的秒数、聚类输出哈希、线程数与 BPF 映射，不记录逐线程的公平性分布，
因此本报告没有公平性跨度这一项。

## 5 无效与重跑

三个 runner 合计 60 次运行（LevelDB 24、mutexbench 24、streamcluster 12），全部一次通过：
没有超时、非零退出、映射不符、缺 BPF fd、线程数不足、`enable_seq` 不加一、日志报错或
`dmesg` watchdog 行，`attempt` 全为 1，没有重跑，没有 cell 用尽尝试次数。三个
`summary.json` 的 `invalid_runs` 与 `cells_without_valid_sample` 都是空表。

会话开始前另做了一次 preflight（`--preflight`，tip arm、`mcs_tas_accordin`、5 秒
fillrandom），有效，247.052 Kops/s；它只用于确认构建与输入，不并入上面任何均值。

## 结论

- **mutexbench 持平。** 四个配置的 tip/base 为 0.977、1.012、1.000、1.007，每个差值都不超过
  同一 arm 三次运行的跨度；噪声水平为 CV 0.1%–3.8%。
- **LevelDB readrandom 变差。** tip/base 为 0.905（`mcs_accordin`）与 0.948
  （`mcs_tas_accordin`），两个后端的逐轮值都完全不重叠，噪声水平为 CV 0.5%–1.9%，
  差值是噪声的 5 倍以上。
- **LevelDB fillrandom 变好。** tip/base 为 1.029（`mcs_accordin`，逐轮值不重叠）与 1.044
  （`mcs_tas_accordin`，两 arm 在边缘相接，差值与 base 自身 4.47% 的 CV 同量级）。
- **streamcluster 变好。** tip/base 为 0.971（`mcs_accordin`，与 base 的 2.61% CV 同量级）
  与 0.968（`mcs_tas_accordin`，逐轮值不重叠）；噪声水平为 CV 0.5%–2.6%。
- 这次改动同时把加载文本压到 0.782 倍、把验证器处理量抬到 2.146 倍。readrandom 跟着后者走：
  加载文本减少 603 条的同时它掉了 9.5%，因此「几百条加载文本值 10–15% readrandom」这条
  敏感性不能只按加载文本计量。fillrandom 与 streamcluster 在同一份二进制上朝相反方向动，
  所以两个量对四个负载不是同一个方向的影响，本次数据不足以给出超出这一点的归因。

## 数据与脚本

- [mutexbench 逐轮数据](mutexbench/results.csv)、[JSONL](mutexbench/results.jsonl)、
  [汇总](mutexbench/summary.json)、[表格](mutexbench/summary.md)、
  [配置](mutexbench/metadata.json)
- [LevelDB 逐轮数据](leveldb/results.csv)、[JSONL](leveldb/results.jsonl)、
  [汇总](leveldb/summary.json)、[表格](leveldb/summary.md)、[配置](leveldb/metadata.json)
- [streamcluster 逐轮数据](streamcluster/results.csv)、[JSONL](streamcluster/results.jsonl)、
  [汇总](streamcluster/summary.json)、[表格](streamcluster/summary.md)、
  [配置](streamcluster/metadata.json)
- [指令数实测](instructions.json)
- [运行日志、构建日志与聚类输出归档](logs.tar.gz)
- runner：[mutexbench.py](mutexbench.py)；LevelDB 与 streamcluster 用
  [cv-custody 的 runner](../cv-custody-20260906/run.py) 与
  [其 streamcluster runner](../cv-custody-20260906/streamcluster.py)，未作改动。

在仓库根目录重测（每个脚本自己获取共用 flock，不要再套一层 `flock`）：

```sh
sudo python3 docs/benchmarks/cv-custody-20260906/run.py \
  --arm bpfbase=fbc463e:ACCORDIN_CV_COUNTERS=0 --arm bpftip=432bee6:ACCORDIN_CV_COUNTERS=0 \
  --repeats 3 --benchmarks readrandom,fillrandom --backends mcs_accordin,mcs_tas_accordin \
  --out target/bpf-volume-repeat/leveldb
sudo python3 docs/benchmarks/bpf-volume-20260909/mutexbench.py \
  --arm bpfbase=fbc463e:ACCORDIN_CV_COUNTERS=0 --arm bpftip=432bee6:ACCORDIN_CV_COUNTERS=0 \
  --repeats 3 --duration-ms 5000 --warmup-ms 1000 --out target/bpf-volume-repeat/mutexbench
sudo python3 docs/benchmarks/cv-custody-20260906/streamcluster.py \
  --arm bpfbase=fbc463e:ACCORDIN_CV_COUNTERS=0 --arm bpftip=432bee6:ACCORDIN_CV_COUNTERS=0 \
  --repeats 3 --modes stream --out target/bpf-volume-repeat/streamcluster
```

---

# 二分：readrandom 的损失落在哪个提交，以及 `0082ed7`

上面的 A/B 把 `fbc463e` 与 `432bee6` 之间的三个提交当成一次改动来测，得到 readrandom
0.905x 与 0.948x。这一节把三个提交拆开逐个测，并把 `0082ed7`（把 admission 辅助函数内联回
调用点、保留全部删减）一起放进同一场，回答两件事：损失出现在哪一个提交上，`0082ed7` 是否
把它收回。

arm 与提交：`bpfbase`=`fbc463e`、`subprog`=`be0dd22`、`flags`=`2a872ce`、`bpftip`=`432bee6`、
`inline`=`0082ed7`。所有比值都是 arm/`bpfbase`。本节的数字只与本节内部比较，不与上面五节的
数字比较：那是另一场会话，机器是共用的。

## 6 配置

- 主机、内核、governor、被测程序、固定环境与上面五节相同，`ACCORDIN_CV_COUNTERS=0` 仍然加在
  每一个 arm 上。
- 五个 arm 各自在 `target/cv-custody-20260906/{bpfbase,subprog,flags,bpftip,inline}-src` 的
  detached worktree 里检出并构建（`make -j all` 与 `make litl`）。`bpfbase-src` 与 `bpftip-src`
  是上面五节用过的同两个 worktree，本次重新构建后四个产物的 SHA-256 与上面的表逐字相同，
  因此两场会话跑的是同一份二进制。三个新 arm 的产物：

| 文件 | subprog (`be0dd22`) | flags (`2a872ce`) | inline (`0082ed7`) |
|---|---|---|---|
| `target/release/libmcs_accordin_direct.so` | `d0ec3afb3c37dada9579f2a2d16d89261a1e57bf65d0f5e06d6c6fd85508bd9b` | `9cee72f55580c63fd6804d55fa6cc5b999072ac6d0c8254b88347b390c9a06d6` | `e2703f7a812c6b21a3f94f172549aa0355cc004610afb52cc2aa5af4b935ca44` |
| `target/release/libmcs_tas_accordin_direct.so` | `dc44c4a11c0001f2d724edef42291c059a44c88c34a34a92b8e65101009000e5` | `ac2c2aedec8accbd37971ba787173e8a9933e5e2d063ead0f762b414ddb2a691` | `2023d02ae540e998622f7dee8779731824f1686148042b30cdac1eca1d6530e7` |
| `third_party/litl/lib/libmcsaccordin_original.so` | `a60e8dd6d7250253208e60c2e3ca4e412460ac84293a3eeac298369dfde40da3` | `b2c26196bb4f1395257f1a5f3890a04d59e05c9009e84912dd9269ed9d66bf24` | `57e1be7a46af9645c94c6ca7819c51809ff6309dcaaa58a9cbd901767a77dbd7` |
| `third_party/litl/lib/libmcstasaccordin_original.so` | `067f46563c86e8585f7006e1a6cae7f570e351e9c2c88200fdaca15ab2e7d81e` | `25ade77bd0d791fc1b194c5449c596aa1d80f811fe032c046ddd8a413404d763` | `7264e72328e554749393f2f9043ade1faf3c165fc935e3895e79e086e5ed012e` |

- 编译器仍为 `clang 21.1.8` 与 `cc 14.3.0`，48 个逻辑 CPU 全部在线，governor 为 performance。
- 四个 runner 依次串行执行，全程持有 `/tmp/mutexbench-sweep-multi-lock.lock`，每次加载调度器前
  等待 `/sys/kernel/sched_ext/state` 为 `disabled`，任何时刻只有一次调度器加载。

## 7 指令数，五个提交

口径与第 1 节相同：「加载」是一个两线程 `mutex_bench` 把调度器挂上之后 `bpftool prog show`
报出的 xlated 指令数与 JIT 字节数，「验证器处理」来自 `ACCORDIN_VERIFY_ONLY=1`。两项都在同一
把锁下、`ACCORDIN_CV_COUNTERS=0` 下重测。`bpfbase` 与 `bpftip` 的三项总计与第 1 节逐位相同
（2,765/12,584/23,866 与 2,162/10,085/51,222）。

加载指令数：

| 程序 | bpfbase | subprog | flags | bpftip | inline |
|---|---:|---:|---:|---:|---:|
| dispatch | 733 | 384 | 384 | 384 | 728 |
| cv_flush | 423 | 433 | 361 | 340 | 340 |
| init | 337 | 337 | 337 | 326 | 322 |
| enqueue | 327 | 309 | 307 | 281 | 301 |
| dump | 249 | 247 | 247 | 206 | 206 |
| stopping | 182 | 156 | 145 | 144 | 176 |
| tick | 158 | 158 | 147 | 146 | 152 |
| exit | 118 | 118 | 118 | 113 | 113 |
| exit_task | 102 | 102 | 102 | 92 | 92 |
| yield | 78 | 83 | 72 | 72 | 72 |
| select_cpu | 42 | 42 | 42 | 42 | 42 |
| runnable | 8 | 8 | 8 | 8 | 8 |
| quiescent | 8 | 8 | 8 | 8 | 8 |
| 合计 | 2,765 | 2,385 | 2,278 | 2,162 | 2,560 |

验证器处理指令数：

| 程序 | bpfbase | subprog | flags | bpftip | inline |
|---|---:|---:|---:|---:|---:|
| dispatch | 14,976 | 42,457 | 42,457 | 42,670 | 46,639 |
| cv_flush | 3,311 | 3,250 | 3,275 | 3,221 | 3,221 |
| init | 3,056 | 3,115 | 3,115 | 3,021 | 3,026 |
| enqueue | 591 | 533 | 517 | 484 | 534 |
| dump | 477 | 483 | 483 | 387 | 387 |
| stopping | 362 | 274 | 350 | 349 | 405 |
| tick | 319 | 259 | 315 | 314 | 358 |
| exit_task | 317 | 317 | 317 | 323 | 323 |
| exit | 305 | 305 | 305 | 313 | 313 |
| yield | 88 | 95 | 81 | 77 | 77 |
| select_cpu | 50 | 50 | 50 | 49 | 49 |
| runnable | 7 | 7 | 7 | 7 | 7 |
| quiescent | 7 | 7 | 7 | 7 | 7 |
| 合计 | 23,866 | 51,152 | 51,279 | 51,222 | 55,346 |

三项总计与相对 `bpfbase` 的比值：

| arm | 提交 | 加载指令 | 比值 | JIT 字节 | 比值 | 验证器处理 | 比值 |
|---|---|---:|---:|---:|---:|---:|---:|
| bpfbase | `fbc463e` | 2,765 | 1.000 | 12,584 | 1.000 | 23,866 | 1.000 |
| subprog | `be0dd22` | 2,385 | 0.863 | 11,120 | 0.884 | 51,152 | 2.143 |
| flags | `2a872ce` | 2,278 | 0.824 | 10,661 | 0.847 | 51,279 | 2.149 |
| bpftip | `432bee6` | 2,162 | 0.782 | 10,085 | 0.801 | 51,222 | 2.146 |
| inline | `0082ed7` | 2,560 | 0.926 | 11,682 | 0.928 | 55,346 | 2.319 |

`be0dd22` 一步就把 `ops.dispatch` 的加载文本从 733 压到 384、把它的验证器处理量从 14,976 抬到
42,457；后两个提交各再削一百多条加载文本，验证器处理量不再动。`0082ed7` 把 dispatch 的加载
文本还原到 728，同时把它的验证器处理量推到五个 arm 里最高的 46,639，总计 55,346。

## 8 readrandom 二分，192 线程

`db_bench --benchmarks=readrandom --threads=192 --time_ms=30000`，键空间 1,000,000、
value 100 B，`--use_existing_db=1`，每轮从同一份 fillseq 种子复制独立副本并逐文件核对大小与
SHA-256，会话开始时对种子做一次只读扫描，数出 1,000,000 项。每个 arm 每个后端三次，
每轮内 arm 轮转起始位置。

| arm | 提交 | 后端 | 三次 Kops/s | 均值 Kops/s | CV | arm/bpfbase |
|---|---|---|---|---:|---:|---:|
| bpfbase | `fbc463e` | mcs_accordin | 1369.627 / 1347.120 / 1334.062 | 1350.270 | 1.33% | — |
| subprog | `be0dd22` | mcs_accordin | 1142.376 / 1186.177 / 1156.876 | 1161.810 | 1.92% | 0.860 |
| flags | `2a872ce` | mcs_accordin | 1341.935 / 1339.459 / 1339.970 | 1340.455 | 0.10% | 0.993 |
| bpftip | `432bee6` | mcs_accordin | 1166.620 / 1201.811 / 1193.791 | 1187.407 | 1.55% | 0.879 |
| inline | `0082ed7` | mcs_accordin | 1346.007 / 1335.494 / 1352.331 | 1344.611 | 0.63% | 0.996 |
| bpfbase | `fbc463e` | mcs_tas_accordin | 1192.520 / 1158.023 / 1172.039 | 1174.194 | 1.48% | — |
| subprog | `be0dd22` | mcs_tas_accordin | 1047.799 / 1094.951 / 1074.754 | 1072.501 | 2.21% | 0.913 |
| flags | `2a872ce` | mcs_tas_accordin | 1167.702 / 1150.535 / 1193.805 | 1170.681 | 1.86% | 0.997 |
| bpftip | `432bee6` | mcs_tas_accordin | 1054.577 / 1063.521 / 1074.035 | 1064.044 | 0.92% | 0.906 |
| inline | `0082ed7` | mcs_tas_accordin | 1189.480 / 1184.986 / 1140.570 | 1171.678 | 2.31% | 0.998 |

五个 arm 的逐轮值分成两簇，两个后端上分法相同。`mcs_accordin`：快簇
（bpfbase、flags、inline）的最低值 1334.062 高于慢簇（subprog、bpftip）的最高值 1201.811；
`mcs_tas_accordin`：快簇最低 1140.570 高于慢簇最高 1094.951。快簇三个 arm 的逐轮区间互相重叠，
均值差为 0.3%–0.7%（`mcs_accordin`）与 0.1%–0.3%（`mcs_tas_accordin`），与各 arm 自身
0.1%–2.3% 的 CV 同量级；慢簇两个 arm 的逐轮区间同样重叠。两簇之间的均值差为 8%–14%。

## 9 `fbc463e` 与 `0082ed7` 的另外三项

readrandom 上 `inline` 回到 `bpfbase` 的 0.996 倍，因此按同样的条件补测 fillrandom、
mutexbench 与 streamcluster，只留 `bpfbase` 与 `inline` 两个 arm。

fillrandom，`--threads=192 --time_ms=30000`，每轮新路径、`--use_existing_db=0`：

| 后端 | arm | 三次 Kops/s | 均值 Kops/s | CV | inline/bpfbase |
|---|---|---|---:|---:|---:|
| mcs_accordin | bpfbase | 239.123 / 272.538 / 240.782 | 250.814 | 7.51% | — |
| mcs_accordin | inline | 243.475 / 236.209 / 258.539 | 246.074 | 4.63% | 0.981 |
| mcs_tas_accordin | bpfbase | 246.059 / 265.504 / 261.914 | 257.825 | 4.01% | — |
| mcs_tas_accordin | inline | 239.686 / 270.342 / 257.656 | 255.895 | 6.02% | 0.993 |

mutexbench，`--lock-kind mutex`，`--duration-ms 5000 --warmup-duration-ms 1000`，后端
`mcs_accordin`，配置命名为 `线程数-临界区纳秒-临界区外纳秒`：

| 配置 | arm | 三次 Kops/s | 均值 Kops/s | CV | inline/bpfbase |
|---|---|---|---:|---:|---:|
| low-t2-c100-o0 | bpfbase | 3566.5 / 3589.0 / 3476.1 | 3543.9 | 1.69% | — |
| low-t2-c100-o0 | inline | 3492.9 / 3579.6 / 3490.7 | 3521.0 | 1.44% | 0.994 |
| low-t4-c100-o0 | bpfbase | 3976.5 / 3980.0 / 4019.8 | 3992.1 | 0.60% | — |
| low-t4-c100-o0 | inline | 4023.0 / 4016.1 / 3993.3 | 4010.8 | 0.39% | 1.005 |
| low-t4-c100-o3000 | bpfbase | 1075.1 / 1061.3 / 1066.5 | 1067.7 | 0.65% | — |
| low-t4-c100-o3000 | inline | 1069.0 / 1072.9 / 1068.4 | 1070.1 | 0.23% | 1.002 |
| overload-t96-c100-o3000 | bpfbase | 2585.3 / 2602.8 / 2601.5 | 2596.5 | 0.38% | — |
| overload-t96-c100-o3000 | inline | 2533.5 / 2601.1 / 2605.4 | 2580.0 | 1.56% | 0.994 |

streamcluster，`streamcluster 10 30 512 32768 32768 2000 none <output> 192`，单位秒，越低越快。
12 次运行的聚类输出 SHA-256 全部为
`13b33997906352993b3d67c0d86cf2a984741f96163597990e935d3f72cc5b84`：

| 后端 | arm | 三次秒 | 均值秒 | CV | inline/bpfbase |
|---|---|---|---:|---:|---:|
| mcs_accordin | bpfbase | 35.798 / 34.368 / 34.808 | 34.992 | 2.09% | — |
| mcs_accordin | inline | 34.157 / 33.864 / 35.530 | 34.517 | 2.58% | 0.986 |
| mcs_tas_accordin | bpfbase | 35.182 / 35.426 / 34.926 | 35.178 | 0.71% | — |
| mcs_tas_accordin | inline | 34.471 / 34.933 / 33.538 | 34.314 | 2.07% | 0.975 |

三项的每一个差值都落在同一 arm 三次运行自身的跨度里：fillrandom 两个后端的 1.9% 与 0.7%
对着 4.0%–7.5% 的 CV，mutexbench 四个配置的 0.2%–0.6% 对着 0.2%–1.7% 的 CV，streamcluster
两个后端的 1.4% 与 2.5% 对着 0.7%–2.6% 的 CV，且两个 arm 的逐轮区间在三项八个单元里全部重叠。

## 10 无效与重跑

四个 runner 合计 78 次运行（readrandom 30、fillrandom 12、mutexbench 24、streamcluster 12），
全部一次通过：没有超时、非零退出、映射不符、缺 BPF fd、线程数不足、`enable_seq` 不加一、
日志报错或 `dmesg` watchdog 行，`attempt` 全为 1，没有重跑，没有 cell 用尽尝试次数。四个
`summary.json` 的 `invalid_runs` 与 `cells_without_valid_sample` 都是空表。

## 二分结论

- **掉的是 `be0dd22` 与 `432bee6` 两个 arm。** readrandom 比值：`mcs_accordin` 上
  0.860（`be0dd22`）与 0.879（`432bee6`），`mcs_tas_accordin` 上 0.913 与 0.906。
  夹在中间的 `2a872ce` 与最后的 `0082ed7` 都不掉：0.993 与 0.996（`mcs_accordin`）、
  0.997 与 0.998（`mcs_tas_accordin`）。两簇的逐轮值在两个后端上都不重叠。
- **变化不是随提交顺序单调的。** `be0dd22` 掉，`2a872ce` 回到基线，`432bee6` 又掉，
  `0082ed7` 再回到基线。因此这次数据不支持「某一个提交单独带来了那 9.5%」这种说法：
  能说的是五个二进制分成不重叠的两簇，慢簇是 `be0dd22` 与 `432bee6`。
- **`0082ed7` 把 readrandom 收回。** 0.996（`mcs_accordin`）与 0.998（`mcs_tas_accordin`），
  两个后端的逐轮区间都与 `bpfbase` 重叠，差值小于两个 arm 各自的 CV。补测的另外三项也没有
  代价：fillrandom 0.981 与 0.993、mutexbench 0.994/1.005/1.002/0.994、streamcluster
  0.986 与 0.975，每个差值都在同 arm 三次运行的跨度内。
- **加载文本与验证器处理量都解释不了这次的分簇。** `2a872ce`（加载 2,278、验证器 51,279）
  与 `432bee6`（2,162、51,222）两项几乎相同却分处快慢两簇；`0082ed7` 的验证器处理量是五个
  arm 里最高的 55,346（`ops.dispatch` 46,639），readrandom 却与 `bpfbase` 齐平。上面第 5 节
  「readrandom 跟着验证器处理量走」的说法在扩到五个提交后不成立，本次数据也不足以给出
  替代的归因。

## 续测数据与脚本

- [readrandom 二分逐轮数据](readrandom-bisect/results.csv)、[JSONL](readrandom-bisect/results.jsonl)、
  [汇总](readrandom-bisect/summary.json)、[表格](readrandom-bisect/summary.md)、
  [配置](readrandom-bisect/metadata.json)
- [fillrandom 逐轮数据](fillrandom-inline/results.csv)、[汇总](fillrandom-inline/summary.json)、
  [表格](fillrandom-inline/summary.md)、[配置](fillrandom-inline/metadata.json)
- [mutexbench 逐轮数据](mutexbench-inline/results.csv)、[汇总](mutexbench-inline/summary.json)、
  [表格](mutexbench-inline/summary.md)、[配置](mutexbench-inline/metadata.json)
- [streamcluster 逐轮数据](streamcluster-inline/results.csv)、[汇总](streamcluster-inline/summary.json)、
  [表格](streamcluster-inline/summary.md)、[配置](streamcluster-inline/metadata.json)
- [五个提交的指令数实测](instructions-bisect.json)
- [运行日志、构建日志与聚类输出归档](logs-bisect.tar.gz)

在仓库根目录重测（每个脚本自己获取共用 flock，不要再套一层 `flock`）：

```sh
ARMS="--arm bpfbase=fbc463e:ACCORDIN_CV_COUNTERS=0 --arm subprog=be0dd22:ACCORDIN_CV_COUNTERS=0 \
  --arm flags=2a872ce:ACCORDIN_CV_COUNTERS=0 --arm bpftip=432bee6:ACCORDIN_CV_COUNTERS=0 \
  --arm inline=0082ed7:ACCORDIN_CV_COUNTERS=0"
sudo python3 docs/benchmarks/cv-custody-20260906/run.py $ARMS \
  --repeats 3 --benchmarks readrandom --backends mcs_accordin,mcs_tas_accordin \
  --out target/bpf-volume-repeat/readrandom-bisect

PAIR="--arm bpfbase=fbc463e:ACCORDIN_CV_COUNTERS=0 --arm inline=0082ed7:ACCORDIN_CV_COUNTERS=0"
sudo python3 docs/benchmarks/cv-custody-20260906/run.py $PAIR \
  --repeats 3 --benchmarks fillrandom --backends mcs_accordin,mcs_tas_accordin \
  --out target/bpf-volume-repeat/fillrandom-inline
sudo python3 docs/benchmarks/bpf-volume-20260909/mutexbench.py $PAIR \
  --repeats 3 --duration-ms 5000 --warmup-ms 1000 \
  --out target/bpf-volume-repeat/mutexbench-inline
sudo python3 docs/benchmarks/cv-custody-20260906/streamcluster.py $PAIR \
  --repeats 3 --modes stream --backends mcs_accordin,mcs_tas_accordin \
  --out target/bpf-volume-repeat/streamcluster-inline
```
