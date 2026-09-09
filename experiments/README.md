# 短临界区与线程过载实验

六个应用配置统一比较 `mcs / mcs-tas / gcr / flexguard / mcs-tse / accordin`。
Accordin 使用当前工作区的 `libmcs_tas_accordin_direct.so` 和标准 LiTL pthread 适配器，
BPF、admission、CV custody 开启；不引用旧 Rust 后端或历史实验的预编译库。

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
当前机器 P=96，因此线程数是 **24、48、96、192、384**。若 P 不能被 4 整除，
P/4、P/2 向下取整且最小为 1，重复点去重。可用 `--cpus 0-23` 将实验限定为一个
24 核分区，此时 P=24；可用 `--threads` 选择诊断子集，该选项不改变 P。

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
若修改生成补丁或 baseline 编译选项，请用新的 `--build` 目录重新构建；第三方副本在
补丁成功后固定，普通重复执行用于继续构建及重新构建当前 Accordin/LiTL 源码。

- MCS/MCS-TAS/FlexGuard 来自 FlexGuard commit
  `951c9417393574d5918c08b5f54ecca0df18d872`。使用仓库已有 ARM64 移植补丁；
  MCS/MCS-TAS 的队列发布和交接补充 acquire/release 原子操作，保留队列策略。
- GCR 使用 `bench/otherlocks/gcr_mcs.c` 和其 pthread 适配器，是仓库现有 GCR-on-MCS 实现。
- FlexGuard 为 `LOCK_VERSION=FLEXGUARD HYBRID_VERSION=MCS`，BPF 开启，
  `CONDVARSWAIT=BLOCK ADD_PADDING=1 DEBUG=0`。
- MCS-TSE 为同一 MCS 加 `bench/mutexbench` 已有的 **rseq slice extension** 支持，
  require 模式；包含嵌套临界区计数及独立 `tse_probe`。当前 ARM64 6.14 内核/用户态 ABI
  不支持该扩展，明确记录 `unsupported`，不会降级成普通 MCS。FlexGuard 旧式
  `/sys/kernel/extend_sched` 接口在本机也不存在；新脚本采用仓库现有 rseq 路径。
- Accordin 默认参数显式固定：group=8、own_limit=0、own_slack=100 us、custody=20 ms、
  flush_flags=8（MOVE）、flush_width=0、auto admission=0。对应当前运行时默认值。

六种后端都只替换 mutex/条件变量，保留 libc rwlock、spinlock、barrier；FlexGuard
系列通过导出符号表限制。Streamcluster 的 PARSEC barrier 自身用 mutex/cond 实现，
因此仍由被测锁保护。每种实现的条件变量机制并不完全相同，结果是整套适配器的比较。
同一负载的所有锁共用同一应用二进制。
GCR 保留仓库的默认 active_limit=1、signal_period=16384、passive_spins=1024，
其现有 pthread 适配器的 trylock 总返回 EBUSY；涉及 trylock 的应用不能将它视为
完整 POSIX mutex 实现。这六个固定配置经过实际试跑，未据此承诺其他 API/工作负载兼容。

计时补丁只增加聚合结果输出、LevelDB 固定工作量/单线程 cache 预热，以及 Streamcluster
`CLOCK_MONOTONIC` 纳秒计时，避免把 ARM counter 当作 x86 GHz 换算。上游来源：
[LevelDB](https://github.com/google/leveldb/tree/1.23)、
[RocksDB](https://github.com/facebook/rocksdb/tree/v9.10.0)、
[RocksDB db_bench](https://github.com/facebook/rocksdb/wiki/Benchmarking-tools)、
[PARSEC 输入](https://github.com/cirosantilli/parsec-benchmark/releases/tag/3.0)。

## 结果解释与失败处理

`results.jsonl` 保留每次尝试的命令、环境、线程峰值、加载库、BPF fd、sched_ext
enable_seq、退出码、总 wall time、ROI 时间、吞吐量和错误原因。Accordin 必须实际观察到
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

`summary.csv` / `summary.json` 包含中位数、最小/最大值、CV、成功次数、超时/失败次数：

- `retention_vs_P = rate(T) / rate(P)`：同一锁在过载后的性能保留比例；2P/4P 显著下降
  是崩溃现象的应用级证据，应同时看有效工作量和失败记录。
- `speedup_vs_mcs = rate(lock,T) / rate(mcs,T)`：同线程数下相对 MCS 的加速比。
- 超时、错误和不支持的样本没有吞吐量，空值不当作 0；缺少基线时不计算加速比。
  有部分超时的中位数是成功样本的条件统计，必须连同超时次数一起报告。

超时会终止整个进程组并保留日志，仍继续其余矩阵点；不自动重试慢点，也不跳过更高
线程数。默认任何 timeout/error/invalid/unsupported 都令最终退出码非零。
`--allow-unsupported` 仅允许已明确记录的不支持 baseline，不掩盖其他失败。
`--resume` 保留所有已记录尝试，包括失败；需要重新测量时使用新结果目录。
