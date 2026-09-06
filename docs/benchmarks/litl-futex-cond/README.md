# Accordin LiTL 直接 futex 条件变量

后续变更：已移除 `COND_VAR` 构建/测试开关，当前版本始终启用直接 futex
条件变量。下文 0/1 对照及相关命令记录的是移除开关之前的实验，不再作为
当前配置入口。

移除开关后，`make check-litl` 和 BPF 下 192 线程测试均通过；BPF 测试特意
保留了旧环境值 `COND_VAR=0`，条件变量测试仍完整执行。旧 make 参数也不再
影响动态库内容。此次验证日志保存在 `target/litl-unconditional-cond/`。

2026-09-05。按要求，标准 LiTL 中的 `mcsaccordin_original` 和
`mcstasaccordin_original` 共用直接 futex 条件变量，移除了 shadow pthread
mutex。其它上游锁及 FlexGuard 源码未修改。

## 实现

- `third_party/litl/src/accordin.c`：普通 lock/trylock/unlock 只调用 direct
  API。mutex 内部对象只含一个指针，没有 shadow 或条件变量启用标志。
- `third_party/litl/src/accordin-cond.c`：在条件变量内部维护等待队列。先
  登记 waiter，再释放业务锁，最后用 `FUTEX_WAIT_BITSET_PRIVATE` 等待。
  每个 waiter 的 futex 字位于其栈上；早到的 signal 会先设置该字，随后
  futex 的值检查阻止错误入睡。signal 选择一个已登记 waiter，broadcast
  选择全部 waiter，不累计供未来 waiter 消费的通知。
- 条件变量内部的自旋锁仅保护队列；普通 mutex 操作不接触它。唤醒方持有
  队列锁完成 `FUTEX_WAKE_PRIVATE`，等待方退出前也获取该锁，避免栈上 futex
  在唤醒 syscall 完成之前被释放或复用。
- 支持 realtime/monotonic 绝对超时、静态初始化和 deferred cancellation。
  排队和业务锁重入期间禁用取消，仅在不持有业务锁/队列锁的 futex 等待区间
  临时启用异步取消，让原始 syscall 可以响应取消。cleanup 先移除 waiter，
  必要时把已消费的 signal 补偿给另一个 waiter，再恢复业务锁。
- 接管 `pthread_cond_clockwait@GLIBC_2.30` 和 `@@GLIBC_2.34`，覆盖现代
  `std::condition_variable::wait_for`。动态库使用 `-z now`，避免在可取消区间
  首次解析 PLT 符号。参见 Linux 手册中的
  [futex 绝对超时语义](https://man7.org/linux/man-pages/man2/FUTEX_WAIT_BITSET.2const.html)
  和 [取消状态/类型控制](https://man7.org/linux/man-pages/man3/pthread_setcancelstate.3.html)。

这与仓库其它直接 futex 锁采用相同的“登记—解锁—等待—重新加锁”方式。
显式等待队列用于保留当前适配器需要的超时和取消行为。

只支持进程内普通 mutex 和条件变量；进程共享条件变量返回 `ENOTSUP`，
仍有 waiter 时销毁返回 `EBUSY`。mutex 的其它限制见仓库 README。
`COND_VAR=0` 返回 `ENOTSUP`，`COND_VAR=1` 启用上述完整条件变量实现。
两者普通 mutex 的对象布局与操作路径相同；本机编译出的两个后端的 mutex
对象文件，在 0/1 两种配置之间逐字节一致。

## mutexbench 300,3000

96 个 CPU（0–95）、192 线程、单锁、CS/NCS 300/3000 ns，校准 9/32，
采样步长 8。预热 1 秒、测量 5 秒，BPF/admission 开启、stats-only/TSE 关闭。
同一 benchmark 二进制和 direct 动态库；适配器 `-O3 -g`，不定义 `NDEBUG`。
每个配置、每个后端运行两次，共 12 次。下表是算术平均值：

| 后端 | 直接调用 ops/s | LiTL COND_VAR=0 ops/s | LiTL COND_VAR=1 ops/s | 1 相对 0 |
| --- | ---: | ---: | ---: | ---: |
| MCS | 1,315,247.79 | 1,290,580.45 | 1,296,907.65 | +0.49% |
| MCS-TAS | 1,327,930.45 | 1,363,470.69 | 1,358,598.70 | -0.36% |

`COND_VAR=1` 相比直接调用，MCS 为 -1.39%，MCS-TAS 为 +2.31%。这些是
两次重复的结果；MCS-TAS direct 两次为 1,357,782.75 和 1,298,078.14 ops/s，
有明显轮次波动，因此不能据此宣称经 LiTL 比直接调用更快。

本次 `COND_VAR=0/1` 的吞吐已接近。普通锁路径不再承受此前 shadow 的额外
开销。这是没有条件变量等待的 mutex 微基准，未测量条件变量操作本身的吞吐。

配置顺序：direct、0、1、0、direct、1，每组先 MCS，再 MCS-TAS。
各次检查了实际加载的 LiTL/direct 库、BPF 启用状态、enable_seq 恰好增加
一次、退出后 sched_ext 为 disabled，以及全部 192 个线程操作数非零且
求和等于总操作数。各配置的 benchmark 和 direct 库 SHA-256 一致。

逐次结果见 [runs.csv](runs.csv)，均值和标准差见 [summary.json](summary.json)，
二进制指纹见 [sha256.json](sha256.json)。本机 `target/litl-futex-cond/`
保存源码、构建配置、两种配置的库和对象文件、测试日志、运行清单及汇总脚本。
各原始运行目录记录完整命令、实际加载映射和线程操作数。

## 验证

- `make check-litl`：两个后端均通过，8 线程、各 80,000 次混合 lock/trylock。
- `sudo make check-litl-bpf LITL_TEST_THREADS=192 LITL_TEST_ITERATIONS=2000`：
  两个后端均通过，各 384,000 次计数操作；并发首次等待、192 waiter 的
  50 轮 broadcast、signal、超时、错误返回、取消与 signal 竞态全部通过。
- `sudo taskset -c 0 make check-litl-bpf EXTERNAL_CFLAGS=-DNDEBUG
  LITL_TEST_ITERATIONS=1000`：单 CPU 下两个后端完整通过。
- `make check-litl COND_VAR=0`，随后恢复 `COND_VAR=1` 并重新检查：均通过。
- 新增测试覆盖无 waiter 时的通知、负数 deadline、取消模式保留、busy destroy、
  32 轮取消与 signal 竞态的通知补偿、clockwait 两种 glibc 版本，以及 C++
  `wait` / `wait_for` / `wait_until`。
- 专项 `NDEBUG` 测试限制 mutex 对象只含 direct 指针，将 native mutex
  函数指针设为失败陷阱，验证普通 lock/trylock 和条件变量等待后仍不调用它们。

上述对照实验结束时构建为 `COND_VAR=1`、`-O3 -g`。当前已删除该开关，
直接 futex 条件变量始终启用。
