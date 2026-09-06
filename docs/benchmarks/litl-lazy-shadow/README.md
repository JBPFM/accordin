# LiTL shadow mutex 按需启用

本页为历史优化实验。当前标准 LiTL 集成已按用户要求移除按需启用和缓存行
隔离，当前 `COND_VAR=0/1` 对比见 [shadow 开销测量](../litl-shadow-cost/README.md)。
本页数字对应保留的优化源码及动态库快照，不对应当前默认构建。

2026-09-05，在 AArch64 / 96 CPU 主机上，`COND_VAR=1` 的纯 mutex 路径
已接近直接调用 Accordin：四轮均值中 MCS 低 1.55%，MCS-TAS 低 2.71%。

| 后端 | 直接调用 ops/s | LiTL ops/s | LiTL 相对差异 |
| --- | ---: | ---: | ---: |
| MCS | 1,308,304.77 | 1,287,999.18 | -1.55% |
| MCS-TAS | 1,395,436.62 | 1,357,587.42 | -2.71% |

这是当前 `mutexbench 300,3000` 纯 mutex 工作负载的结果；使用过条件变量
等待的 mutex 仍承担 shadow 的开销。测量没有关闭条件变量支持。

## 实现

适配器仍创建原生 shadow pthread mutex，但在该 mutex 第一次
`pthread_cond_wait` / `pthread_cond_timedwait` 前，不在普通 lock、trylock、
unlock 中获取或释放它。第一次等待时，调用者持有 direct 锁，先获取 shadow，
再将 `shadow_active` 设为 1，然后进入原有等待协议。该状态由 direct 锁保护，
无需额外原子操作，并持续到 mutex 销毁。

任何后继线程都必须先取得 direct 锁，才能读取状态；因此首次等待释放 direct
后，后继线程会获取 shadow，无法穿过 libc 的入队及原子释放 shadow 的窗口。
等待正常返回、超时、错误返回和延迟取消，均恢复 direct + shadow 持锁状态。
启用后 shadow 争用时，trylock 仍回滚 direct 并立即返回 `EBUSY`。

保留了上一版的缓存行隔离：shadow 偏移 128 字节，适配器对象 256 字节。
本次减少的是从未等待过条件变量的 mutex 的运行开销，没有减少分配空间。

## 测量方法

本页原始数据采自迁移前的 FlexGuard LiTL 适配器。当前集成已迁至官方 LiTL
的 `third_party/litl`；下方复现命令使用新目录，原始 CSV、日志位置和哈希保留
原样。迁移后验证见 [标准 LiTL 集成](../litl-upstream/README.md)。

同一个 mutexbench 二进制、同一组 direct 动态库。两种入口分别为：

- 直接调用：没有 `LD_PRELOAD`，`--lock-kind mcs_accordin_direct` 或
  `mcs_tas_accordin_direct`，通过对应 `*_DIRECT_LIB` 加载 direct 库。
- LiTL：`--lock-kind mutex`，预加载对应 `libmcsaccordin_original.so` 或
  `libmcstasaccordin_original.so`；构建配置 `-O3 -g`、`COND_VAR=1`。

共同参数：192 线程、CPU 0–95、单锁、CS/NCS 300/3000 ns、预热 1000 ms、
测量 5000 ms、校准 9/32、计时采样步长 8，BPF/admission 开启，stats-only
和 timeslice extension 关闭。每种入口、每个后端各四次，共 16 次运行。
入口顺序为 direct、LiTL、LiTL、direct、direct、LiTL、LiTL、direct；
每组内先 MCS，再 MCS-TAS。

| 后端 / 入口 | 四次 ops/s |
| --- | --- |
| MCS direct | 1,299,699.90 / 1,315,520.90 / 1,296,356.02 / 1,321,642.26 |
| MCS LiTL | 1,304,556.38 / 1,276,180.78 / 1,285,440.93 / 1,285,818.61 |
| MCS-TAS direct | 1,401,562.55 / 1,361,053.69 / 1,415,612.10 / 1,403,518.15 |
| MCS-TAS LiTL | 1,338,244.05 / 1,408,623.53 / 1,338,444.30 / 1,345,037.81 |

四次算术平均用于描述当前工作负载，不能据此认定所有线程数和临界区长度都
有相同差距。运行期间检查实际加载的动态库、sched_ext enabled 状态及
enable_seq；每次退出后 scheduler 为 disabled。192 个线程均有进展，线程
操作数之和与总操作数一致。所有正式对比的二进制/direct 库哈希一致。

逐次数据和原始日志位置见 [runs.csv](runs.csv)，汇总含标准差见
[summary.json](summary.json)，二进制指纹见 [sha256.json](sha256.json)。
工作区 `target/litl-lazy-shadow/` 保留运行清单、测试日志和优化后源码/动态库
快照；每次运行目录保存完整命令、元数据、日志及加载映射。

复现基本命令（在仓库根目录，Bash，当前机器的 CPU 0–95）：

```bash
make litl
g++ -O3 -std=c++20 -pthread bench/mutexbench/mutex_bench.cpp \
    -o target/mutex_bench_litl_compare -ldl
bench_args=(--workload single --threads 192 --critical-ns 300 --outside-ns 3000
    --duration-ms 5000 --warmup-duration-ms 1000
    --timing-sample-stride 8 --timeslice-extension off)
bench_env=(MCS_ACCORDIN_DIRECT_DISABLE_BPF=0 MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF=0
    MCS_ACCORDIN_DIRECT_STATS_ONLY=0 MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY=0
    ACCORDIN_DISABLE_ADMISSION=0)

sudo -n taskset -c 0-95 env -u LD_PRELOAD "${bench_env[@]}" \
    MCS_ACCORDIN_DIRECT_LIB="$PWD/target/release/libmcs_accordin_direct.so" \
    target/mutex_bench_litl_compare --lock-kind mcs_accordin_direct "${bench_args[@]}"
sudo -n taskset -c 0-95 env "${bench_env[@]}" \
    LD_PRELOAD="$PWD/third_party/litl/lib/libmcsaccordin_original.so" \
    target/mutex_bench_litl_compare --lock-kind mutex "${bench_args[@]}"

sudo -n taskset -c 0-95 env -u LD_PRELOAD "${bench_env[@]}" \
    MCS_TAS_ACCORDIN_DIRECT_LIB="$PWD/target/release/libmcs_tas_accordin_direct.so" \
    target/mutex_bench_litl_compare --lock-kind mcs_tas_accordin_direct "${bench_args[@]}"
sudo -n taskset -c 0-95 env "${bench_env[@]}" \
    LD_PRELOAD="$PWD/third_party/litl/lib/libmcstasaccordin_original.so" \
    target/mutex_bench_litl_compare --lock-kind mutex "${bench_args[@]}"
```

## 正确性验证及 MCS 修复

优化后的第一次 BPF 测试在普通锁计数阶段超时。保存的 GDB 堆栈显示 MCS
持有者在 `queue_release` 等待后继链接，其他线程在等待锁。检查发现原
`raw_trylock` 发布队列节点的 CAS 只有 acquire，无法保证 `node->next = NULL`
先于后继线程写入链接；在弱内存序机器上存在覆盖后继链接的风险。
已改为 acquire-release，修复节点发布顺序，并增加直接 ABI 的混合
lock/trylock 节点复用压力测试。后续测试全部通过。

所有正式性能对比均使用修复后的 direct 库；最初未修复版本的一次直接调用
基线不计入上表。

通过的检查：

- `make check` 和 `sudo make check-bpf`：两个 direct ABI、嵌套锁、线程清理，
  以及新增的每个后端 160,000 次混合 lock/trylock 操作。
- `make check-litl` 和 `sudo make check-litl-bpf`：两个适配器的完整测试。
- BPF、192 线程：每个后端 384,000 次计数操作，32 个新 mutex 的首次等待
  与并发 lock/trylock，以及 signal、broadcast、超时、错误和取消恢复持锁。
- `-DNDEBUG`、单 CPU、8 线程的完整 LiTL BPF 测试。
- 单独以 `-DNDEBUG` 编译的测试验证：首次等待前不获取 shadow，第一次等待
  后持续获取 shadow，以及 shadow 独占时 trylock 非阻塞返回和 direct 回滚。

工作区最终构建恢复为默认 `COND_VAR=1`、`-O3 -g`。
