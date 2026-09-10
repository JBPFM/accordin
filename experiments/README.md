# 短临界区与线程过载实验

六个应用配置统一比较 `mcs / mcs-tas / gcr / flexguard / mcs-tse / accordin`。
六种锁都是 `third_party/litl` 中的算法，经同一 LiTL 前端接入被测进程，配置之间只差
锁算法本身。Accordin 使用当前工作区的 `libmcs_tas_accordin_direct.so`，BPF、admission、
CV custody 开启；不引用旧 Rust 后端或历史实验的预编译库。

已经实际构建、试跑；见 [本机验证记录](VALIDATION.md)。

## 运行

在仓库根目录执行：

```sh
# 需要 Python >= 3.11、GCC/G++、Clang/LLVM、make、cmake、git、m4、patch、
# bpftool、pkg-config，以及 libbpf/libelf/zlib/libnuma/gflags/gtest/gmock 开发包。
python3 experiments/prepare.py --jobs 12

# 查看机器检测和完整矩阵，不需要 root，也不需要先构建。
python3 experiments/run.py --dry-run

# 六个负载 × 六种锁 × 五个线程档位，各试跑一次。
sudo -n python3 experiments/run.py --profile smoke --out results/overload-smoke

# 正式工作量，每个配置独立预热一次、测量三次，默认每次进程上限 600 s。
sudo -n python3 experiments/run.py --profile full --out results/overload-full

# 单独试跑正式参数：保留相同 P，只挑选部分线程档位。
sudo -n python3 experiments/run.py --profile full --workloads streamcluster raytrace \
  --threads 96 192 --repeats 1 --warmups 0 --out results/parsec-pilot

# 中断后续跑：参数和构建 manifest 必须与原会话相同。
sudo -n python3 experiments/run.py --profile full --out results/overload-full --resume

# 从原始记录重新生成统计，不运行应用。
python3 experiments/run.py --out results/overload-full --report-only
python3 experiments/plot.py results/overload-full  # 可选，需要 matplotlib；输出 PNG/SVG
python3 -m unittest discover -s experiments -p 'test_*.py'
```

默认 P 是进程 affinity 内的物理核数量，每个物理核只选一个硬件线程，所有配置固定使用
这一组 CPU。不会随线程数缩小 CPU 集合，也不会只给 Accordin 绑定更少 CPU。
当前机器 P=48（2 socket × 24 核、无 SMT），因此线程数是 **12、24、48、96、192**。若 P 不能被 4 整除，
P/4、P/2 向下取整且最小为 1，重复点去重。可用 `--cpus 0-23` 将实验限定为一个
24 核分区，此时 P=24；可用 `--threads` 选择诊断子集，该选项不改变 P。

同一配置（负载 × 锁 × 线程数）只要有一次尝试超时，该配置剩下的尝试就不再执行，
无论是预热还是测量、也无论随机顺序把它们排在超时之前还是之后，一律记为 `skipped`。
已经记录的结果保留不变，`--resume` 会从原始记录中读回超时配置并继续沿用该规则。

所有测量串行执行，持有 `/tmp/mutexbench-sweep-multi-lock.lock`，拒绝与已有
sched_ext 调度器叠加。每轮随机化配置顺序，默认固定随机种子 20260909。
脚本不会改变 governor、NUMA 策略或系统服务；主机状态和负载写入 `host.json`。
正式性能测量应在空闲机器上执行。

## 正式参数及用途

| 负载 | 固定参数 | 主要同步路径 |
|---|---|---|
| LevelDB readrandom | 1.23；100,000 keys；16 B key、32 B value；总计 1,966,080 reads；256 MiB block cache、4 KiB block；先 fillseq+compact 建库，再在每个进程内 readseq 预热 | DB 元数据及 block cache 的短 mutex 临界区 |
| LevelDB fillrandom | 相同 key/value；固定总计 983,040 puts；256 MiB write buffer；无压缩；非同步 WAL 位于 tmpfs | writer queue、memtable 写入；减少磁盘等待及中途 compaction |
| Streamcluster | FlexGuard 原配置：`10 30 512 32768 32768 2000 none clusters.txt T` | PARSEC 集中式 pthread mutex/cond barrier，保留 automatic drop-in、关闭 spin barrier |
| Raytrace | FlexGuard 原配置：SPLASH2x simsmall `car.env`，128×128，`-pT -a8`，原 block=8×8、bundle=4×4 | 工作池队列、任务分配 mutex |
| Kyoto Cabinet CacheDB | 真正 `kyotocabinet::CacheDB`；100,000 keys；16 B key、32 B value；总计 1,966,080 ops；90% random get、10% 同长度 set；保留 16 个原始 slot | 内存 cache slot mutex 和 LRU 更新，无 TreeDB 或文件数据库 |
| RocksDB | v9.10.0；readrandom；100,000 keys；16 B key、32 B value；总计约 1,966,080 reads；256 MiB **LRU cache、1 shard**；4 KiB block；关闭压缩/WAL；`readonly=1, open_files=1000`；readtocache 预热 | 有意选择有争用的短 LRU cache 临界区 |

RocksDB 的 `--reads` 是每线程次数，脚本用 `floor(total/T)`，将实际总量记录为
`expected_ops`。默认五档都能整除上述总量。LevelDB 的 `--total_ops` 是实验补丁，
精确分配余数，保持 key space 不变；不会让 4P 比 P/4 多做 16 倍工作。
数据库和中间文件在独立 `/dev/shm/accordin-overload-*` 目录中，结束后只清理本次数据。
随机读使用完整顺序填充的 seed，避免随机填充造成大量未命中。

RocksDB 的单 shard 是显式的热点配置，不能将结果推广到默认分片数或 HyperClockCache。
其只读配置避免 writable DB 的后台定时任务；`open_files=1000` 同时避免启用
CompactedDB 快路径（该路径仍会启动定时任务）。这是必要的应用配置约束：FlexGuard
系列原适配器的 timedwait 直接退出进程。本实验保留这些原实现，不支持用它们执行
含定时条件等待的任意 RocksDB 写入或后台任务。只读路径也省略部分 superversion 操作，
因此该实验的重点是缓存锁，而不是完整读写 DB 的所有同步路径。
Kyoto 使用 FlexGuard 仓库中的 CacheDB 版本，其 slot 字段为 `Mutex`；不额外将 16 slot
改成单锁。内存中的随机读仍会更新 LRU，不是无锁只读测试。
Streamcluster 的 barrier broadcast、条件变量重获锁和唤醒成本也包含在结果中；
fillrandom 还包含 WAL/写入分组成本。它们是应用级压力实验，不能仅凭吞吐曲线就宣称
所有临界区都恒定为某个纳秒值，或将全部退化唯一归因于持锁者被抢占。

`smoke` 只验证可运行性：数据库 4096 keys，总 reads/CacheDB ops=49,152，puts=24,576；
Streamcluster 为 `10 20 16 512 512 1000`，Raytrace 保留正式参数。默认不做额外预热轮、
每配置一次、30 s 进程上限。短运行受初始化和线程启动影响，不能作为最终性能排名。

Streamcluster 在 384 线程时，即使原始程序可以正常完成，也可能超过这一预算：
其 smoke 仍包含 9,758 次两阶段 barrier，累计约 746 万次条件等待。
定位步骤、修复尝试及保留原预算的诊断入口见
[Streamcluster 超时定位](STREAMCLUSTER_TIMEOUT.md)。

## baseline 与构建

`prepare.py` 在 `target/experiments/` 构建应用和 baseline 的私有副本，保存 build logs
及 `manifest.json`。复用已存在的指定版本源代码和 PARSEC 输入；缺失时从相应上游获取。
源版本、源码哈希、原始输入哈希、应用/锁库哈希及已有工作区修改均记录在 manifest。
manifest 的 `sha256` 只含被测产物（应用二进制、锁库、ldd 依赖、原始输入、探针、
补丁与 `sources/`）；`experiments/*.py` 驱动脚本单独记在 `drivers_sha256`，
`source_sha256` 记录源码快照。
若修改生成补丁或 baseline 编译选项，请用新的 `--build` 目录重新构建；第三方副本在
补丁成功后固定，普通重复执行用于继续构建及重新构建当前 Accordin/LiTL 源码。

六种锁都是 LiTL 算法，共用 `src/interpose.c` 前端，且编译开关一致：`NO_INDIRECTION=1`、
`NEED_CONTEXT=0`、`SUPPORT_WAITING=0`、`WAITING_ORIGINAL`；算法实例指针存放在被拦截的
`pthread_mutex_t` 中；`pthread_spin_*` / `pthread_rwlock_*` 不拦截，直接走 glibc 原生实现；
`pthread_mutex_timedlock` 一律返回 `ENOTSUP`。

五个 baseline（mcs、mcs-tas、mcs-tse、gcr、flexguard）另外共用 `src/directlock.c` /
`src/directcond.c`：条件变量是被拦截 `pthread_cond_t` 内的 futex 序列（`pthread_cond_clockwait`
一并拦截，因此 C++ `std::condition_variable` 也留在库内），`directlock_mutex_init` 忽略
mutex 属性。Accordin 不用这两个文件：它链接 `interpose.o + accordin.o + accordin-cond.o`，
条件变量是自己的每等待者 futex FIFO 队列，唤醒要与调度器的 custody / flush 协同（被调度器
扣住的等待者由通知方的批量 flush 释放，其余等待者转入 mutex 的 parking 队列等待 relock
交接），这是 `directcond.c` 的通用实现无法提供的；`accordin_mutex_init` 会校验 mutex 属性，
非 NORMAL / PROCESS_PRIVATE / STALLED / PRIO_NONE 返回 `ENOTSUP`。

`prepare.py` 在私有副本 `<build>/litl` 中一次构建全部六个算法（传 `FLEXGUARD=1`），
manifest 的锁路径直接指向 `<build>/litl/lib/lib<algo>.so`。仓库根目录的
`make litl-baselines` 和 `make check-litl-baselines` 构建并测试树内的 direct 算法；
flexguard 需要额外的运行时归档，只有加 `FLEXGUARD=1` 才会进入这两个目标。

| 实验锁名 | LiTL 算法 | 说明 |
|---|---|---|
| `mcs` | `mbmcs_original` | mutex 微基准的 MCS |
| `mcs-tas` | `mbmcstas_original` | 带 test-and-set 快路径的 MCS |
| `mcs-tse` | `mbmcstse_original` | `mbmcs` 加 rseq slice extension |
| `gcr` | `gcr_original` | MCS 队列之上的 generic concurrency restriction |
| `flexguard` | `flexguard_original` | FlexGuard（SOSP'25），链接其运行时归档 |
| `accordin` | `mcstasaccordin_original` | 链接当前工作区的 MCS-TAS direct 库 |

- MCS/MCS-TAS/MCS-TSE 是 `bench/mutexbench` 的实现移入 LiTL 后的版本，算法未改；
  队列节点和每次获取的状态从进程内共享的 `thread_local` 改为按 (线程, 锁) 私有存储，
  使一个线程可以同时持有多把同类锁。
- MCS-TSE 在临界区前后请求并归还 rseq slice extension。LiTL 版本在内核不提供该扩展时
  静默变为无操作，因此 `run.py` 先运行独立的 `tse_probe`（与库使用同一份
  `include/mbtimeslice.hpp` 检测逻辑），不可用时把 mcs-tse 的全部配置记为 `unsupported`，
  不会当成普通 MCS 测量。本机内核不提供该扩展。
- GCR 是仓库原有的 GCR-on-MCS 实现（源码移入 LiTL），按论文配置编译：active_limit=4、
  rejoin_limit=2（由 max(active_limit/2, 1) 导出）、signal_period=16384 (0x4000)、
  passive_spins=1024。passive 队列头不 park，而是自旋等待批准标志，并按 1 起步、逐次
  翻倍（上限 1<<20）的间隔采样 num_active，读到低于 rejoin_limit 即重新加入 active 集合；
  release 路径里的周期性批准是一次普通 store，临界区内没有 futex 调用。三个参数可用
  `GCR_MCS_ACTIVE_LIMIT`、`GCR_MCS_SIGNAL_PERIOD`、`GCR_MCS_PASSIVE_SPINS` 覆盖，
  每进程解析一次并在 stderr 打印生效值；`run.py` 的 `clean_env` 会清掉 `GCR_` 前缀的
  变量，实验因此总是跑编译期默认值。
- FlexGuard 链接由 FlexGuard 私有副本构建的 `libsync.a`，该副本已应用
  `experiments/patches/flexguard-arm.patch`；编译参数为 `HYBRID_VERSION=MCS`、
  `ADD_PADDING`、`NOBPF=0`，与 FlexGuard 自身默认一致。锁、队列节点和 BPF 运行时都在
  归档内，首次拦截 mutex 时启动，`run.py` 据此检查 BPF fd。不再传 `CONDVARSWAIT=BLOCK`：
  条件变量改由 LiTL 的 `directcond.c` 提供，FlexGuard 自己的 interpose 条件变量路径
  不参与本实验。
- Accordin 默认参数显式固定：group=8、own_limit=0、own_slack=100 us、custody=20 ms、
  flush_flags=8（MOVE）、flush_width=0、auto admission=0。对应当前运行时默认值。

### 已知差异

- `directcond.c` 的 signal 取票号判断该不该唤醒，但唤醒本身走 futex 等待队列顺序，不是票号
  顺序：同一个 condvar 上有两个及以上 sleeper 时，一次 signal 可能唤醒票号不匹配的等待者，
  该次通知因此丢失。这一实现继承自 FlexGuard 的条件变量，只影响五个 baseline，不影响
  accordin。六个 workload 基本使用每写者一个 condvar 或 broadcast，实际未观察到该路径。
- trylock：gcr 与 `mb*` 系列（mcs、mcs-tas、mcs-tse）没有非阻塞入口，`directalgo_trylock`
  一律返回 `EBUSY`，轮询 trylock 直到成功的程序在这四种锁下不会推进；flexguard 与
  accordin 的 trylock 是真实的试获取。
- `mbmcstse` 在每次 acquire / release 上调用 slice extension 的 enter / exit，没有嵌套计数，
  因此一个线程同时持有两把该锁时，释放内层锁就会归还 extension。除了 `run.py` 启动前的
  `tse_probe` 门控，运行时若 `rseq_slice_yield` 返回“不支持”，该线程会把 extension 关掉，
  之后按普通 MCS 运行。
- `mb*` 系列的每 (线程, 锁) context 由 `directlock_context` 分配后不释放：队列节点可能在
  分配它的线程退出后仍被他人引用，因此保留到进程退出。

六种后端都只替换 mutex/条件变量，保留 libc rwlock、spinlock、barrier。
Streamcluster 的 PARSEC barrier 自身用 mutex/cond 实现，因此仍由被测锁保护。
同一负载的所有锁共用同一应用二进制。这六个固定配置经过实际试跑，未据此承诺其他
API/工作负载兼容。

计时补丁只增加聚合结果输出、LevelDB 固定工作量/单线程 cache 预热，以及 Streamcluster
`CLOCK_MONOTONIC` 纳秒计时，避免把 ARM counter 当作 x86 GHz 换算。上游来源：
[LevelDB](https://github.com/google/leveldb/tree/1.23)、
[RocksDB](https://github.com/facebook/rocksdb/tree/v9.10.0)、
[RocksDB db_bench](https://github.com/facebook/rocksdb/wiki/Benchmarking-tools)、
[PARSEC 输入](https://github.com/cirosantilli/parsec-benchmark/releases/tag/3.0)。

## 结果解释与失败处理

`results.jsonl` 保留每次尝试的命令、环境、线程峰值、加载库、BPF fd、sched_ext
enable_seq、退出码、总 wall time、ROI 时间、吞吐量和错误原因。每行还记录
`driver_sha256`，即 manifest `drivers_sha256` 中全部驱动脚本哈希拼接后的 sha256，
覆盖 `run.py` 和 `litl_locks.py` 等全部驱动，用于区分驱动版本。Accordin 必须实际观察到
目标 DSO、direct 库、BPF fd、启用状态及 enable_seq 恰好增加一次。FlexGuard 必须观察到
BPF fd，其余锁的 sched_ext enable_seq 不得变化；每次退出必须卸载调度器。
观察器在启动前 0.2 秒每 1 ms 采样一次，之后每 20 ms 一次，所有锁使用相同规则。
这会产生少量旁路负载，尤其满核短运行应结合这一测量开销解读；默认未保留专用监控核。

LevelDB/RocksDB 的吞吐量采用 **全部完成操作数 / 最早开始到最晚结束的 wall time**，
不使用 `1 / micros-per-op` 冒充多线程总吞吐量。Streamcluster/Raytrace 使用内部 ROI，
以 `runs/s=1/ROI秒数` 比较。每个有效数据库样本必须完成预期操作数；LevelDB 每次随机读
检查命中，CacheDB 检查每次读的 value 及最终数据库。Streamcluster 校验输出维度及
聚类权重总和；Raytrace 校验完整 RLE、每行像素数和尺寸。`-a8` 的共享随机状态使图像
可能随调度变化，结构验证不是确定性的逐像素证明。

`summary.csv` / `summary.json` 包含中位数、最小/最大值、CV、成功次数、超时/跳过/失败次数：

- `retention_vs_P = rate(T) / rate(P)`：同一锁在过载后的性能保留比例；2P/4P 显著下降
  是崩溃现象的应用级证据，应同时看有效工作量和失败记录。
- `speedup_vs_mcs = rate(lock,T) / rate(mcs,T)`：同线程数下相对 MCS 的加速比。
- 超时、跳过、错误和不支持的样本没有吞吐量，空值不当作 0；缺少基线时不计算加速比。
  有部分超时的中位数是成功样本的条件统计，必须连同超时次数一起报告。
- `skipped` 是因同一配置已经超时而没有执行的尝试，全局计数和每配置一列都单独统计。
  它既不是完成样本，也不是新的超时，不参与中位数；一个配置的 `completed + timeouts
  + skipped + unsupported + failures` 才是它计划内的尝试次数。

超时会终止整个进程组并保留日志，仍继续其余矩阵点；不自动重试慢点，也不跳过更高
线程数，但同一配置剩余的尝试记为 `skipped` 且不再执行，因此超时点只被实测一次。
默认任何 timeout/error/invalid/unsupported 都令最终退出码非零；`skipped` 本身不影响
退出码，引发它的那次超时已经令退出码非零。
`--allow-unsupported` 仅允许已明确记录的不支持 baseline，不掩盖其他失败。
`--resume` 保留所有已记录尝试，包括失败；需要重新测量时使用新结果目录。
`--resume` 要求参数以及被测产物与原会话完全一致：manifest `sha256` 中的全部条目
在运行前后各校验一次，任何一项改变即报错。`experiments/*.py` 驱动脚本只作为
provenance 记在 `drivers_sha256`，不参与校验，可以在续跑之间改动，改动后新写出的行
`driver_sha256` 随之变化。
