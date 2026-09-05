# Rust → C 迁移：mutexbench 192 线程

2026-09-04（America/Denver；UTC 2026-09-05）在同一台机器、同一个 mutexbench 二进制上交替运行迁移前后的 direct 库。每个后端、每种实现各运行 5 次，以下为吞吐量中位数：

| 后端 | Rust（ops/s） | C（ops/s） | C 相对 Rust | Rust 最小–最大 | C 最小–最大 |
| --- | ---: | ---: | ---: | ---: | ---: |
| MCS admission direct | 465,926.95 | 461,791.35 | −0.89% | 454,271.70–503,341.08 | 438,776.13–472,565.03 |
| MCS-TAS admission direct | 503,665.70 | 539,678.22 | +7.15% | 500,837.49–505,025.38 | 537,067.39–549,007.66 |

MCS 的差异小于本次运行波动，未显示明确改善或退化；MCS-TAS 的 C 版在本次所有运行中均高于 Rust 版。结论限于下面的单锁负载与机器，不代表其他线程数、CS/NCS 比例或多锁负载。这是完整迁移前后实现的比较，也包含编译器、TLS 实现和 libbpf 版本的差别，不能将全部变化归因于语言。

## 与 results-tmp 中旧记录的核对

用户确认的参考文件为 `bench/mutexbench/results-tmp/mcs_tas_accordin/summary.csv`，核对时的内容保存为 [reference-summary.csv](reference-summary.csv)。它使用平均吞吐量，下面也将当前结果按平均值统计：

| 数据 | 线程数 | 平均吞吐量（ops/s） |
| --- | ---: | ---: |
| results-tmp 旧记录 | 96 | 479,814.39 |
| results-tmp 旧记录 | 128 | 465,111.59 |
| results-tmp 旧记录 | 256 | 502,825.52 |
| 本次 Rust 基线 | 192 | 503,463.46 |
| 本次 C 实现 | 192 | 542,173.92 |

旧表的 `total_operations` 是约 3 秒内的总操作数，例如 96 线程的 1,444,174 次；每秒吞吐量应读取 `mean_throughput_ops_per_sec`，该行约为 48 万 ops/s。当前 C 实现同样处于每秒约 50 万次的范围，没有显示出相对此表的明显下降。但旧表没有 192 线程记录，也未保存完整运行配置或二进制版本，因此它不能提供严格的同线程、同版本条件对照。

## 配置与方法

- CPU：HiSilicon TaiShan-v110，双路、96 个物理核，无 SMT；affinity 为 `0-95`。192 个工作线程相当于两倍超订阅，允许在该范围内迁移。
- 内核：Ubuntu `6.14.0-37-generic`，AArch64。
- 单锁 workload，CS `300 ns`、NCS `3000 ns`；每次预热 1 秒，测量 5 秒，timing sample stride 为 8，timeslice extension 关闭。
- CS/NCS 是 mutexbench 请求的 burn 时间，使用其编译默认校准 `9/32`，并非实际持锁时间的保证。实际持锁时间、操作数和每线程计数保留在日志中。
- BPF 和 admission 均启用，stats-only 关闭。奇数轮先 Rust 后 C，偶数轮先 C 后 Rust；一次只启动一个调度器。
- 使用 benchmark 自身输出的 `throughput_ops_per_sec`，其中 elapsed 包含停止后的 worker 收尾；本次实际 elapsed 为 `5.024670–5.042779 s`。
- 20 次均正常完成。runner 检查调度器加载日志、`enable_seq` 和每 100 ms 的 sched_ext 状态采样，并确认运行后调度器已退出。结果目录保留每轮命令和原始日志。
- 未隔离整台共享机器的其他活动；逐轮记录了 `/proc/loadavg`。本次比较采用交替顺序和重复运行降低时间漂移影响，没有进行统计显著性检验。

## 版本与迁移范围

Rust 基线来自提交 `695c873d1228f12c526e1fe3096bf12169851237`，在改动前以 `cargo build --locked --offline --release` 构建并保存。工具链为 Rust `1.93.1`、scx_utils/scx_cargo `1.0.22`、libbpf-sys `1.6.1+v1.6.1`。

C 版使用 Clang `19.1.7`、`-O3 -g` 和系统 libbpf `1.5.0`；BPF 使用同一个 Clang、`-O2 -mcpu=v3`，由 bpftool `7.6.0` 生成 skeleton。scx C 头文件来自同一个 scx 版本，固定在 `third_party/scx/`；`vmlinux.h` 从本机 BTF 生成。核心构建不再需要 Cargo、Rust、scx_cargo 或 libbpf-rs。

`src/bpf/main.bpf.c`、map/interface 定义和公开 C 头文件保持原样。用户态改为 C11 原子操作、TLS 注册和 scx C 的 `SCX_OPS_OPEN/LOAD/ATTACH`；保留请求编号、每 CPU 一个新等待者名额、嵌套锁及 MCS 兼容别名。MCS 节点池简化为 4 个节点与所属锁指针，继续支持非栈顺序解锁；AArch64 自旋保持 `isb`。不含 vendored 头文件和 BPF 的自有用户态源代码由 1,062 行 Rust 缩为 425 行 C/头文件。

mutexbench 子模块基于 `98904ce1e143dd2ac36aa0d87ebc625ec4d940bb`，使用任务开始时已有的本地修改，两组共用同一个二进制。用于本次单锁 benchmark 的源码差异记录在 [mutexbench-source.patch](mutexbench-source.patch)；没有修改 benchmark 源码来适配迁移。

构建和验证通过：独立输出目录的全量 C/BPF 构建、两个 direct ABI 检查、无 BPF smoke、BPF 单 CPU/双 CPU smoke、等待期间 affinity 变更检查。复用现有 C smoke，没有新增 Rust 单元测试的对应测试框架。

## 复现

当前工作区已保留本次 Rust 二进制于 `target/c-migration-192/rust/`。使用相同配置重新比较（输出目录必须尚不存在）：

```sh
make -j
make -C bench/mutexbench mutex_bench
python3 scripts/compare_mutexbench.py \
  --rust-lib-dir target/c-migration-192/rust \
  --c-lib-dir target/release \
  --threads 192 --cpus 0-95 \
  --critical-ns 300 --outside-ns 3000 \
  --duration-ms 5000 --warmup-ms 1000 --repeats 5 \
  --output target/c-migration-192/recheck
```

runner 使用 `sudo -n` 启动需要 BPF 权限的 benchmark；运行前应无其他 sched_ext 调度器。若已删除本地 Rust 基线，可在独立目录重建：

```sh
git worktree add --detach /tmp/accordin-rust-695c873 695c873d1228f12c526e1fe3096bf12169851237
cargo build --locked --release \
  --manifest-path /tmp/accordin-rust-695c873/Cargo.toml \
  --target-dir "$PWD/target/rust-rebuild"
# 然后将 runner 的 --rust-lib-dir 改为 target/rust-rebuild/release
```

在全新 checkout 中复现 benchmark 源码时，应先在上述 mutexbench 提交上应用保存的 patch，再用其 Makefile 构建。具体机器/工具版本和本次二进制 SHA-256 见 [metadata.json](metadata.json) 与 [baseline.json](baseline.json)；逐次吞吐量、命令及日志文件名见 [results.csv](results.csv)。
