# Shadow mutex 开销与 LiTL 论文的比较口径

历史记录：此处分析的是移除 shadow 之前的实现。当前版本使用
[直接 futex 条件变量](../litl-futex-cond/README.md)。

2026-09-05。前一份报告中的 43.50% / 51.95% 是当前 Accordin 适配器紧凑
布局下 `COND_VAR=1` 相对 `COND_VAR=0` 的吞吐下降，不能直接解释为论文中
标准 LiTL 布局的开销。本次只在 `target/litl-shadow-explanation/` 构建诊断
变体；正式适配器源码、动态库和 direct 库的 SHA-256 在补测前后保持一致。

## 论文实际比较了什么

[LiTL 的 ATC 2016 论文](https://www.usenix.org/system/files/conference/atc16/atc16_paper-guiroux.pdf)
§4.1 说明 shadow 不会引入高争用，并未保证它的吞吐代价很小。§4.3 的平均
差异低于 5%，比较的是三个应用的手工集成与 LiTL 拦截，并非关闭/开启 shadow。
§4.2 明确说明数据结构有缓存行对齐。

上游 [include/mcs.h](https://github.com/multicore-locks/litl/blob/916469ca797ee299a4ae674b41c4fac9ac4ae21b/include/mcs.h)
在 pthread mutex 与 MCS tail 之间加入填充，并对对象对齐；本地
`third_party/litl/include/mcs.h` 保留了该代码。先前移除优化时，也移除了
Accordin 适配器的缓存行隔离，这比“去掉按需 shadow 优化”的范围更大。

本次隔离变体遵循上游的隔离原则，但仍是 Accordin direct API 的适配器，
不能称为上游原版 MCS 或论文实验的完整复现。

## 固定布局补测

四种配置使用同一份当前适配代码，构建参数均为 `-O3 -g`，没有 `NDEBUG`。
shadow 操作始终按 `COND_VAR` 决定，没有首次 wait 才启用 shadow 的逻辑。

| 配置 | COND_VAR | 对象大小 | 对齐 | shadow 偏移 |
| --- | ---: | ---: | ---: | ---: |
| packed0 | 0 | 8 | 8 | 无 |
| packed1 | 1 | 56 | 8 | 8 |
| aligned0 | 0 | 256 | 128 | 128，仅保留空间 |
| aligned1 | 1 | 256 | 128 | 128 |

aligned0 和 aligned1 的对象大小、字段布局和分配方式完全相同，只有后者
初始化并操作 shadow。两者使用 `aligned_alloc`；本机硬件缓存行为 64 字节，
LiTL 的 `L_CACHE_LINE_SIZE` 配置为 128 字节。

192 线程、CPU 0–95、单锁 CS/NCS 300/3000 ns、校准 9/32、采样步长 8，
每次预热 1 秒、测量 5 秒。BPF/admission 开启、stats-only/TSE 关闭。
每个配置、每个后端两次，共 16 次；顺序为 packed0、aligned1、packed1、
aligned0、aligned0、packed1、aligned1、packed0。每组内先 MCS，再 MCS-TAS。

| 配置 | MCS 平均 ops/s | MCS-TAS 平均 ops/s |
| --- | ---: | ---: |
| 紧凑布局，COND_VAR=0 | 1,291,125.16 | 1,380,212.64 |
| 紧凑布局，COND_VAR=1 | 715,189.19 | 651,200.03 |
| 隔离布局，COND_VAR=0 | 1,306,334.46 | 1,390,104.12 |
| 隔离布局，COND_VAR=1 | 874,847.82 | 717,538.49 |
| 紧凑布局，开启 shadow 的吞吐下降 | 44.61% | 52.82% |
| 隔离布局，开启 shadow 的吞吐下降 | 33.03% | 48.38% |

保持 shadow 开启，仅隔离布局，本轮吞吐分别提升 22.32% / 10.19%。这是
两次重复的诊断结果，不是固定的硬件成本。布局解释了部分损失，剩余损失
仍显著。逐次测量见 [runs.csv](runs.csv)，含标准差的结果见
[summary.json](summary.json)。原始日志、加载映射、构建配置和哈希保存在
`target/litl-shadow-explanation/` 及 manifest 指向的运行目录。

每次运行均检查实际加载的诊断库和 direct 库、BPF 启用状态、enable_seq
递增一次、退出后调度器 disabled，以及所有 192 个线程的操作数非零且
求和等于总数。各轮 benchmark 和 direct 库的哈希相同。

## futex 计数

另外对 packed1 的两个后端各运行一次，用 bpftrace 的
`syscalls:sys_enter_futex` tracepoint 按进程名 `mutex_bench` 计数。
两次测量区间合计完成 6,770,891 次操作，整个进程生命周期合计仅记录
16 次 futex 调用（包含初始化和线程退出等调用）。两个进程按 futex 命令
分类的计数分别为：WAIT=1 / WAKE=7 / WAIT_BITSET=3，以及 WAKE=2 /
WAIT_BITSET=3。计数没有进一步按 shadow 地址过滤，因此不能宣称 shadow
精确为零次；但可以排除每轮 shadow 都触发内核等待的解释。

跟踪原文见 [futex-bpftrace.log](futex-bpftrace.log)，运行位置和结果见
[futex-summary.json](futex-summary.json)。这两次带跟踪运行未混入上述吞吐
平均值。此前的 strace 尝试在 45 秒超时，已排除，不用它的计数作结论。

## 为什么没有争抢也会贵

当前实现的顺序为 direct lock → native shadow lock → 用户临界区 →
native shadow unlock → direct unlock。本测试没有条件变量等待，shadow
只由 direct 持锁者访问，因此并不会出现普通的多个线程同时争抢 shadow。

检查本机 glibc 2.41 的反汇编，多线程普通 mutex 的快路径包含 acquire
CAS 和 release exchange 两次原子读改写，此外还有函数调用、类型判断、TLS
读取，以及 owner、nusers 字段的读写。原子 helper 在支持 LSE 时分别使用
`casa`、`swpl`。这是完整的 lock/unlock 对，不是一次本地缓存命中的原子指令。

连续持锁者可以位于不同 CPU/NUMA 节点。即使 CAS 一次成功，shadow 所在
缓存行仍可能需要转移写权限。紧凑布局又把其它线程读取的 direct 指针与
shadow 锁字放在一起，进一步引入无关读写之间的缓存失效。隔离能消除这类
共享，但不能消除 shadow 自身随持锁者转移的共享。

这两次 native 操作都在 direct 持锁期间，延长单锁的串行服务时间。300 ns
的短临界区会放大这一成本的吞吐占比。mutexbench 的 `avg_lock_hold_ns`
从 lock 返回后计时，在 unlock 调用前停止，因此该指标不包含 shadow 的
加锁/解锁时间。不能因该指标基本不变，就认为锁持有区间没有增加。

当前实验使用 ARM、96 核、四个 NUMA 节点、192 个线程及 Accordin 调度。
这些条件也限制了与论文应用实验的可比性。本轮没有独立分解缓存行迁移、
glibc 指令、调度相互作用各自的贡献；不能把剩余 33% / 48% 全部归因于
某一条原子指令，也不能从吞吐倒数差直接推断原子指令延迟。
