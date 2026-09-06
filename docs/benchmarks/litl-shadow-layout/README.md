# LiTL shadow mutex 缓存行隔离

2026-09-05，在 AArch64 / 96 CPU 主机上，将 Accordin LiTL 适配器的只读
`direct` 指针与 shadow pthread mutex 分开缓存行，并使用相应对齐的分配。
shadow 的加锁、解锁、条件变量等待协议不变，所有测量均为 `COND_VAR=1`。

原布局把指针和 shadow 放在一个 56 字节对象中；shadow 的 owner、锁字等字段
更新会使其它线程读取的指针缓存行失效。新布局使用 LiTL 的 128 字节对齐，
shadow 偏移为 128，内部适配对象大小为 256 字节。额外空间是本次优化的代价。

同一 mutexbench 二进制和同一组 direct 动态库，单锁 `--lock-kind mutex` 经
LiTL 替换；192 线程、CPU 0–95、CS/NCS 300/3000 ns、校准 9/32、采样步长 8、
预热 1 秒、测量 5 秒。BPF/admission 开启，stats-only 与 TSE 关闭。
每个版本、每个后端测量两次；顺序为旧版、新版、新版、旧版，各组内先 MCS
再 MCS-TAS。下表是两次运行的算术平均值，不是大规模统计实验。

| 后端 | 原布局 ops/s（两次） | 隔离布局 ops/s（两次） | 平均提升 |
| --- | --- | --- | --- |
| MCS | 750,872.06 / 768,168.38 | 859,714.79 / 857,198.23 | 13.03% |
| MCS-TAS | 622,731.06 / 658,489.41 | 754,390.54 / 720,275.59 | 15.10% |

各次运行的 192 个线程操作数均非零，求和与总操作数一致；检查了实际加载的
LiTL/direct 库、运行期间的 sched_ext enabled 状态、enable_seq，以及退出后
调度器处于 disabled。无 BPF 和 BPF 的 LiTL 测试均通过，包括 signal/broadcast、
超时、错误返回、取消恢复持锁，以及 `NDEBUG` 下的 shadow 争用 trylock 回滚。

本机 libc 的普通 pthread mutex 多线程快路径在加锁时使用 acquire CAS，解锁时
使用 release atomic exchange；另外还读写 mutex 类型、owner 和使用计数等字段。
没有同时竞争的线程仍可能需要跨 CPU 获取缓存行所有权，因此这些操作不能
按一次本地缓存命中的原子指令估算。本次只验证了缓存行隔离带来的收益，未把
剩余开销进一步分解。

原始运行 CSV、汇总、基线/修改后源码与动态库，以及 libc 反汇编保存在工作区
`target/litl-shadow-layout/`。各次原始输出和 SHA-256 元数据位于其 `runs.csv`
记录的 `target/litl-layout-*-condvar1-300-3000-*` 目录。
