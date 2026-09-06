# LiTL Accordin 适配器未隔离布局的 shadow mutex 开销

历史记录：当前两种 Accordin 适配器已切换为
[直接 futex 条件变量](../litl-futex-cond/README.md)，不再使用 shadow mutex。

测量口径更正：以下使用标准 LiTL 源码中的 Accordin 适配器，但适配器的紧凑
布局不等同于上游 MCS 的布局。上游本来就有缓存行对齐和填充；此前把它们也
随按需 shadow 优化一起移除，不适合作为论文实现的完整复现。下面的数值仍是
该紧凑布局下的有效测量。论文对照及固定布局的补测见
[原因分析](../litl-shadow-explanation/README.md)。

2026-09-05，按用户要求移除 `third_party/litl` Accordin 适配器中的按需 shadow
和缓存行隔离优化，测量普通 shadow mutex 支持的性能影响。

`mutexbench 300,3000` 中，开启 shadow 相比 `COND_VAR=0`，MCS 吞吐下降
43.50%，MCS-TAS 下降 51.95%。每个后端、每种配置运行三次，以下为算术平均：

| 后端 | 直接调用 ops/s | LiTL `COND_VAR=0` ops/s | LiTL `COND_VAR=1` ops/s | 开启 shadow 的吞吐下降 |
| --- | ---: | ---: | ---: | ---: |
| MCS | 1,306,132.05 | 1,292,512.38 | 730,217.78 | 43.50% |
| MCS-TAS | 1,355,847.02 | 1,349,610.93 | 648,433.53 | 51.95% |

下降比例使用 `1 - mean(COND_VAR=1) / mean(COND_VAR=0)`。
`COND_VAR=0` 相比直接调用，MCS 低 1.04%，MCS-TAS 低 0.46%。

## 被测实现

标准 LiTL 源码位于 `third_party/litl`，上游基线为
`multicore-locks/litl@916469ca797ee299a4ae674b41c4fac9ac4ae21b`。
两个适配器分别链接当前的 MCS 和 MCS-TAS direct 库。

`COND_VAR=1` 的每次成功 lock/trylock 均获取 direct 和原生 shadow pthread
mutex；unlock 先释放 shadow，再释放 direct。没有按需启用标志，也不在首次
条件变量等待之前跳过 shadow。原生调用的执行不依赖 `assert` 或 `NDEBUG`。

对象恢复为普通 `calloc` 分配的 direct 指针加 shadow mutex，没有额外缓存行
填充。该 AArch64 主机上 `pthread_mutex_t` 为 48 字节，适配器对象 56 字节、
对齐 8 字节、shadow 偏移 8 字节。`COND_VAR=0` 不含 shadow，对象为 8 字节。

两种配置使用相同的适配代码，只有 `COND_VAR` 不同。两者都保留现有
`NO_INDIRECTION` 接入、direct API 及 MCS 节点发布的内存序修复。
本次结果包含 shadow 支持及对象布局/分配大小变化的整体影响，没有单独分解
pthread 调用、原子指令和缓存一致性成本。

## 测量方法

- AArch64、96 个逻辑 CPU，affinity 为 CPU 0–95，192 个工作线程。
- 同一个 mutexbench 二进制、同一组 direct 动态库，适配器使用 `-O3 -g`。
- 单锁，CS/NCS 300/3000 ns，校准 9/32，计时采样步长 8。
- 每次预热 1 秒，测量 5 秒；BPF/admission 开启，stats-only 和 TSE 关闭。
- 直接调用不设置 `LD_PRELOAD`，使用 `--lock-kind mcs_accordin_direct` 或
  `mcs_tas_accordin_direct`；LiTL 使用 `--lock-kind mutex` 和对应预加载库。
- 工作负载没有条件变量等待，因而观察的是 shadow 给普通 mutex 操作增加的成本。

九组运行的配置顺序为：direct、0、1、0、1、direct、1、direct、0。
每组先 MCS，再 MCS-TAS，共 18 次运行。每种配置各自的适配器哈希在重复
运行间一致，benchmark 和 direct 库哈希在所有配置之间一致。

| 后端 / 配置 | 三次吞吐量（ops/s） |
| --- | --- |
| MCS direct | 1,318,024.66 / 1,307,545.73 / 1,292,825.76 |
| MCS `COND_VAR=0` | 1,290,248.15 / 1,302,135.40 / 1,285,153.60 |
| MCS `COND_VAR=1` | 754,837.21 / 710,051.43 / 725,764.71 |
| MCS-TAS direct | 1,358,517.83 / 1,351,389.69 / 1,357,633.55 |
| MCS-TAS `COND_VAR=0` | 1,318,150.44 / 1,358,005.43 / 1,372,676.92 |
| MCS-TAS `COND_VAR=1` | 638,993.19 / 645,437.51 / 660,869.88 |

各次运行均核对了实际加载的 LiTL/direct 库、BPF 启用状态、enable_seq、退出后
调度器为 disabled，以及全部 192 个线程的操作数非零且求和等于总操作数。
没有加载 FlexGuard 的库。

逐次数据及原始日志位置见 [runs.csv](runs.csv)，含标准差的汇总见
[summary.json](summary.json)，二进制指纹见 [sha256.json](sha256.json)。
本机 `target/litl-shadow-cost/` 保留源码、两种构建的动态库和配置快照、运行
清单、测试日志、布局探针及汇总脚本。各运行目录保存完整命令和加载映射。

## 切换构建

在仓库根目录运行：

```sh
make litl COND_VAR=0
make check-litl COND_VAR=0

make litl COND_VAR=1
make check-litl
sudo make check-litl-bpf
```

构建会按配置变化重新编译适配器。测量时使用相同 benchmark 与参数，入口为：

```sh
sudo third_party/litl/libmcsaccordin_original.sh ./mutex_bench \
    --lock-kind mutex --workload single --threads 192 \
    --critical-ns 300 --outside-ns 3000 \
    --warmup-duration-ms 1000 --duration-ms 5000 \
    --timing-sample-stride 8 --timeslice-extension off
```

MCS-TAS 使用 `libmcstasaccordin_original.sh`。实际运行的 CPU affinity、环境
变量和完整二进制路径均记录在原始 `.run.json` 中。

## 正确性验证

- 无 BPF 完整测试：两个后端，各 8 线程、80,000 次混合 lock/trylock。
- BPF、192 线程：各 384,000 次计数操作，32 个新 mutex 的并发首次等待，
  signal/broadcast、两种时钟的超时、错误返回和取消恢复持锁均通过。
- `COND_VAR=0`：mutex 测试及条件变量返回 `ENOTSUP` 均通过。
- BPF、单 CPU、8 线程、`-DNDEBUG`：完整条件变量测试通过。
- 专项测试确认：第一次 condvar wait 之前，普通 lock/trylock 已持有 shadow；
  shadow 独占时 trylock 立即返回 `EBUSY` 并回滚 direct。

最终构建为 `COND_VAR=1`、`-O3 -g`，每次成功加锁都会获取 shadow mutex。
