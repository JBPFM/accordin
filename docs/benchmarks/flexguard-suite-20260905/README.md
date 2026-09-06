# FlexGuard 测试集：Accordin 与 FlexGuard（2026-09-05）

测试已完成：14 个负载配置（含 5 个 LevelDB 模式和补充 Index-4K）× 2 档线程数 × 3 种锁，共 84 个配置。78 个配置各取得三份有效计时样本，6 个配置首次失败后跳过后续重复；合计 **240 次正式尝试、234 份有效计时样本**。预跑和诊断不计入结果。

完整均值与 CV 见 [results.md](results.md)，机器可读结果见 [summary.csv](summary.csv) 和 [raw.csv](raw.csv)，对比图见 [comparison.svg](comparison.svg)。构建和运行目录为 `target/flexguard-suite-20260905/`。

## 主要结果

- **Scheduling 与 LevelDB 随机读有优势。** 相对 FlexGuard，MCS/MCS-TAS 的 Scheduling 性能分别为 1.62×/2.34×（96 线程）、1.74×/2.79×（192）；LevelDB 随机读分别为 1.12×/1.74×、1.59×/2.47×。
- **差距明显依赖负载和线程数。** 96 线程 Buckets 中 MCS/MCS-TAS 仅为 FlexGuard 的 0.038×/0.049×；192 线程变为 0.989×/1.194×。LevelDB 的写入、覆盖和顺序读均是 FlexGuard 更快。
- **Volrend 与 Streamcluster 仍有较大差距。** 96 线程 Volrend 均值 MCS 194.6 s、MCS-TAS 198.1 s、FlexGuard 19.3 s；192 线程为 503.1 s、503.2 s、302.6 s。Streamcluster 的对应均值为 118.4/86.7/19.0 s 和 400.3/364.1/178.8 s。Volrend 的输出格式限制见下文，不能将计时完成等同于图像正确性通过。
- **Dedup 接近。** 两档线程数三种锁的均值差异约在 4% 内；六个首轮配置均通过完整解压和 SHA-256 校验。Raytrace 中 FlexGuard 均值更快，但其 CV 达 21%–26%。
- **索引结果取决于页大小。** 原始 256 B 页 MCS 加载中止；MCS-TAS 相对 FlexGuard 为 0.545×（96）和 2.831×（192）。统一 4 KB 页后三种锁全部跑通，MCS/MCS-TAS 为 1.117×/1.030× 和 1.745×/1.769×。256 B 页 MCS-TAS 的 96 线程 CV 为 16.9%，应保留这项波动。
- **KyotoCabinet 的 Accordin 配置均超过 180 s 上限**，没有可比较的完成吞吐量；FlexGuard 两档线程数各完成三轮。不能把超时记成 0 ops/s，也没有据此判定死锁。

这些结果是当前标准 LiTL、直接 futex 条件变量、BPF/admission 开启时的实测。去掉 shadow mutex 后，Accordin 仍有显著依赖负载的性能差异。

后续 [Streamcluster 192 线程诊断](../streamcluster-analysis-20260905/README.md) 通过保留 BPF、仅关闭 admission 的完整负载消融定位了主要差距；这些诊断数据独立保存，不改写本次三轮基线。

## 验证与数据

[audit.json](audit.json) 核对了全部 84 个配置、17 个程序/库 SHA-256、每份有效样本的库映射、BPF fd、sched_ext 启停序列和 Scheduling 线程点，没有发现证据不一致。测试结束后 sched_ext 为 disabled。通用互斥正确性测试在 96/192 线程下三种锁均通过；应用计时有效不代表所有应用输出均经过语义验证。

- [results.jsonl](results.jsonl)：运行记录，包含预跑、诊断性试跑和正式计时；统计仅使用 `phase=performance`。
- [raw-logs.tar.gz](raw-logs.tar.gz)：原始输出、构建日志、失败日志和 GDB 调用栈；[单独的索引调用栈](index-mcs-backtrace.log)。
- [verification.jsonl](verification.jsonl)：六次 Dedup 完整往返校验；[log-audit.json](log-audit.json)：234 份有效计时日志的错误扫描结果。
- [artifacts.jsonl.gz](artifacts.jsonl.gz)：应用输出大小与 SHA-256；[Volrend 帧统计](volrend-image-checks.json) 中 18 次运行均输出 3000 帧，文件哈希比较不代表像素比较；[格式问题证据](volrend-output-format.json)。
- [metadata.json](metadata.json)：版本、主机、输入和二进制哈希；[interruption.json](interruption.json)：排除并重跑的未完成样本。

仅重新生成表格和图，无需重跑应用：`python3 docs/benchmarks/flexguard-suite-20260905/report.py --plot`。若原构建目录已移除，脚本会读取本目录归档的 results.jsonl。

## 配置

- TaiShan-v110 / AArch64，96 个物理核，无 SMT，2 socket / 4 NUMA node；所有进程允许使用 CPU 0–95。performance governor、boost 关闭，观测 CPU0 为 2.6 GHz；默认 first-touch NUMA 分配，不强制 interleave。每项测试请求 96 / 192 个工作线程，每个可正常完成的配置重复三轮，三轮轮换锁顺序。测试串行运行；主机仍有常驻后台服务。
- `mcs_accordin`、`mcs_tas_accordin`：当前 **标准 LiTL** 中的适配器，链接当前 direct 库；BPF 和 admission 均开启。条件变量直接使用 futex，没有 shadow mutex，没有 COND_VAR 开关。没有修改锁算法或提高节点池上限。
- FlexGuard：现有 `/mnt/sde/jz/flexguard_arm` 的 ARM 适配版本，基于与当前 FlexGuard 子模块相同的提交。`LOCK_VERSION=FLEXGUARD HYBRID_VERSION=MCS BPF ADD_PADDING=1 CONDVARSWAIT=BLOCK DEBUG=0`；不启用 TSE / FLEXGUARD_ALL。使用其 pthread interposer。
- 每个工作负载三种锁使用**同一个动态链接的程序**。微基准通过 `LOCK_VERSION=MUTEX USE_REAL_PTHREAD=0` 编译，确保调用经过 LD_PRELOAD；不使用会直接调用 libc 的 `USE_REAL_PTHREAD=1`。
- 所有 Accordin/SCX/LD 环境变量先清理，再设置明确的 BPF/admission 开关。每个样本保存实际映射的库、BPF fd、sched_ext 状态与 enable_seq。Accordin 必须确认 sched_ext 启用且序列增加一次；FlexGuard 使用其 tracepoint BPF，sched_ext 保持关闭。每轮退出后确认 scheduler 已卸载。
- 版本、主机信息、库和程序 SHA-256 见 [metadata.json](metadata.json)。首次构建补充安装了 NUMA、glog、TBB、KyotoCabinet、gmock 开发包；实际 KyotoCabinet 测试链接的是 FlexGuard 子模块自带源码构建的私有库。

## 负载参数与计时

| 负载 | 参数 / 输入 | 指标 |
|---|---|---|
| Scheduling | 固定线程点；`-b T -n T -s T -i 1 -d 5000 -t 2 -c 100 -l 0` | 原始 ops/ms ×1000 → ops/s；`-c 100` 在源码中是 100 次 NOP 循环，不等于 100 个 ARM counter ticks |
| Buckets | `-n T -d 10000 -b 100 -m 100000 -o 40 -c 0 -p 0` | CS/s |
| LevelDB 1.20 | `readrandom/fillrandom/fillseq/readseq/overwrite`；`--threads=T --time_ms=30000`；默认 100 万键、100 B value；读/覆盖前从相同单线程 fillseq 种子复制；每轮独立 DB，位于 `/tmp` tmpfs | 总完成操作数 / 合并后的实际起止时间；原始 micros/op 是各线程累计时间 / 总操作数，不直接求倒数。readseq 源码忽略 time_ms，每线程遍历至多 100 万项，按实际用时计算 |
| KyotoCabinet | FlexGuard 的 LevelDB tree_db driver + 私有 KyotoCabinet；`--threads=T --num=50000 --benchmarks=fillrandom,readrandom`；每轮独立 DB | 每线程各写/读 50,000 次；合计 4.8M/9.6M 次写入后再执行同量读取；180 s 上限，超时没有吞吐量数值 |
| Raytrace | SPLASH2x simsmall `car.env`，128×128；`-pT -a8 car.env` | 程序报告的不含初始化时间，µs → s |
| Dedup | PARSEC native `FC-6-x86_64-disc1.iso`；`-c -p -wgzip -t31/63` | **3t+2 = 95/191 个工作线程**，另有主线程和库辅助线程；ROI counter ticks / CNTFRQ_EL0。首轮成功结果还进行完整解压和 SHA-256 校验 |
| Volrend | SPLASH2x native `head`；`T head 1000` | 每轴 1000 rotation steps、三轴共 3000 帧；包含源码默认的预处理与输出；程序 Benchmark time µs → s，900 s 上限 |
| Streamcluster | `10 30 512 32768 32768 2000 none output T` | ROI counter ticks / CNTFRQ_EL0；600 s 上限 |
| Index | BTreeLC pthread/std::mutex wrapper，256 B page；PiBench：`--threads=T --mode=time --read_ratio=0 --update_ratio=1 --seconds=10 --records=100000000 --distribution=SELFSIMILAR --skew=0.2 --bulk_load --pcm=false --skip_verify=true --apply_hash=false` | Completed ops / 实际 ROI 时间；加载不计入吞吐量。PiBench 按其逻辑把每个 worker 固定到单个 CPU，192 线程每核两个；600 s 含加载上限 |

ARM 虚拟计数器实测 **100 MHz**，不是 CPU 的 GHz 主频。应用库辅助线程不计入上述请求的工作线程数；采样观察到的进程线程数峰值保存在 raw.csv，短暂存在的线程可能未被采到。

## 适配与限制

- 所有适配均在独立构建副本中完成，`bench/flexguard` 保持未修改；[patches/](patches/) 保存差异。
- 复用已有 ARM FlexGuard 适配：LL/SC 原子路径、队列 acquire/release、ARM 寄存器/PC BPF 判定和 ARM glibc 的 pthread 符号导出。它是 ARM 移植版的实测结果，不是 x86 论文结果的直接复现。
- 修正 Scheduling 的未初始化链表指针（calloc）并固定初始化随机种子；将 x86 rdtsc 换为 ARM counter；修正 test_init 超时条件中的 `||`。这些修改对三种锁相同。
- Dedup 保留断言：其源码在 assert 中执行必要初始化，不能加 `-DNDEBUG`。其他应用使用统一的 Release 构建。Volrend 的旧 libtiff 补齐原 Makefile 中被命令行覆盖的原型/IEEE 浮点宏。
- Volrend 自带旧 libtiff 在 LP64 上将 `TIFFHeader.tiff_diroff` 存为 8 B unsigned long，使文件头长 16 B；实测输出不能被 Pillow 的标准 TIFF 解码器识别。三种锁使用同一原始程序和输出库，本项仅比较该应用工作量的时间，不声称图像文件正确性通过。输出数量和 SHA-256 已记录，原始文件 SHA 不相同，不据此推断渲染像素相同。
- PiBench 关闭 Intel PCM 和 epoch reclamation（本次 update-only 无节点回收），修复 CPU topology 读取缓冲区；BTree 的 x86 pause 改为 ARM yield。使用统一 pthread wrapper，使三种锁的 BTree page 布局相同；这与论文直接静态嵌入 nopad lock 的布局不同。
- 补充 Index-4K 使用仓库已有的 4096 B page 配置，其余参数与 Index 相同，单独列示。页大小同时改变分支数、树高、锁竞争和页内搜索（256 B 为线性搜索，4096 B 为二分搜索），不能将两种页大小之间的差值归因于锁算法。
- MCS-Accordin 当前每线程只有 4 个 MCS 节点。256 B BTree 的批量插入在分裂时同时持有更多祖先锁，两个线程配置均在加载期中止。额外用单线程、关闭 BPF 的 GDB 运行也复现 SIGABRT，调用栈为 `mcs_accordin_direct_mutex_lock → insertPessimistically → bulk_load`，与节点池耗尽的源码路径一致。记录为不支持该配置，不通过修改算法绕过；诊断不计入性能样本。
- Accordin 的 spinlock/rwlock 透传 libc；FlexGuard interposer 会替换它们。尤其 KyotoCabinet 使用 rwlock/内部原子锁，因此该应用不是单纯的 pthread_mutex 算法成本对比。
- 上限内不能完成的配置保留失败/超时记录，后续重复轮跳过。途中为让 runner 跳过已知超时配置，排除了一次未完成的 KyotoCabinet MCS-TAS 样本并重新运行，记录见运行目录的 interruption.json。
- 测量覆盖上述主要独立应用和微基准，不包括原实验脚本所有线程扫描点、并发应用组合、Hackbench、fairness 或 latency 专项。

## 复现

构建需要上面列出的系统依赖以及 root BPF 权限：

```sh
make litl
python3 docs/benchmarks/flexguard-suite-20260905/prepare.py
```

默认创建全新的 `target/flexguard-suite-reproduction/`，不会覆盖已有结果。准备独立种子后串行运行：

```sh
mkdir -p /tmp/accordin-flexguard-suite-20260905
touch /tmp/mutexbench-sweep-multi-lock.lock
./target/flexguard-suite-reproduction/leveldb/out-static/db_bench --benchmarks=fillseq --threads=1 --db=/tmp/accordin-flexguard-suite-20260905/seed
sudo python3 target/flexguard-suite-reproduction/run.py preflight
sudo python3 target/flexguard-suite-reproduction/phase1.py
sudo python3 target/flexguard-suite-reproduction/phase2.py
sudo python3 target/flexguard-suite-reproduction/phase3.py
```

runner 使用 `/tmp/mutexbench-sweep-multi-lock.lock` 串行化测试；该文件须已创建且可读。暂存 DB 名称使用上述固定前缀，因此勿并发运行另一个复制的 suite。结果只给出三轮均值、标准差和 CV；不将三次测试视为严格统计显著性证明。

报告中的性能倍数以 FlexGuard 为 1：吞吐量使用 `Accordin ops/s ÷ FlexGuard ops/s`，耗时使用 `FlexGuard seconds ÷ Accordin seconds`，因此始终是大于 1 表示 Accordin 更快。
