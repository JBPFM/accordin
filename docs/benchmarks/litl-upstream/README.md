# 标准 LiTL 集成验证

2026-09-05，将两个 Accordin 后端迁移到官方
[multicore-locks/litl](https://github.com/multicore-locks/litl) 的源码基础上，
固定提交 `916469ca797ee299a4ae674b41c4fac9ac4ae21b`。
源码位于 `third_party/litl`，来源、导入范围和修改清单见
[UPSTREAM.md](../../../third_party/litl/UPSTREAM.md)。

该目录保留官方原有锁算法、头文件、配置和构建规则，新增 MCS / MCS-TAS
Accordin 适配器及相应测试。使用官方的 `L_CACHE_LINE_SIZE` 命名，补充当前
glibc 符号版本与 AArch64 所需支持。根目录构建和测试入口均已切换至该目录。
FlexGuard 中此前添加的 Accordin 源码改动已撤回，对应生成文件已移到备份，
`git -C bench/flexguard status --porcelain` 为空。

本页记录迁移完成时仍带按需 shadow 优化的版本。之后按用户要求移除了按需
启用和缓存行隔离；当前实现及 `COND_VAR=0/1` 对比见
[标准 LiTL shadow 开销](../litl-shadow-cost/README.md)。以下测量数据保留原样。

## 构建和运行

```sh
make litl
make check-litl
sudo make check-litl-bpf

sudo third_party/litl/libmcsaccordin_original.sh ./program
sudo third_party/litl/libmcstasaccordin_original.sh ./program
```

也可直接在 `third_party/litl` 运行：

```sh
make ALGORITHMS="mcsaccordin_original mcstasaccordin_original"
make check
sudo make check-bpf
make check COND_VAR=0
```

Accordin 目标无需 FlexGuard、CLHT、ssmem 或 PAPI。其他上游算法保留原来的
依赖及平台要求；本次在 AArch64 上验证的是两个 Accordin 后端。

默认仍是 `COND_VAR=1`，保留按需 shadow：第一次条件变量等待前，mutex
只获取 direct 锁；首次等待在持有 direct 时安全启用 shadow，此后一直使用。
条件变量的唤醒、超时、错误返回和延迟取消继续恢复完整持锁状态。

## 正确性验证

以下检查均通过，测试结束后 sched_ext 状态为 disabled：

| 检查 | 规模及覆盖 |
| --- | --- |
| 根目录 `make check-litl` | 两后端、8 线程、各 80,000 次混合 lock/trylock；初始化、嵌套锁、原生 spin/rwlock、condvar、超时、错误和取消 |
| 根目录 `sudo make check-litl-bpf` | 同上，启用 BPF/admission |
| BPF，192 线程、每线程 2,000 次 | 各 384,000 次计数操作，32 个新 mutex 的首次等待与并发 lock/trylock，192 个 broadcast 等待者 |
| LiTL 目录 `make check COND_VAR=0` | 两后端 mutex 测试及 condvar 返回 `ENOTSUP` |
| LiTL 目录 `check-bpf EXTERNAL_CFLAGS=-DNDEBUG`，单 CPU、8 线程 | 完整条件变量测试；每线程计数 1,000 次 |
| shadow 专项测试，`-DNDEBUG` | 按需启用、启用后的持锁行为、shadow 独占时 trylock 立即失败及 direct 回滚 |

同时逐文件对照固定的上游提交，确认所有已有锁算法 `.c` 和对应 `.h` 未修改。
构建产物、生成的 topology 和 launcher 均由 LiTL 的 `.gitignore` 排除。
最终恢复 `COND_VAR=1`、`-O3 -g` 的默认构建。

## 吞吐量复测

使用前次相同的 mutexbench 二进制，并对标准 LiTL 和直接调用重新交替测量。
参数为单锁、192 线程、CPU 0–95、CS/NCS 300/3000 ns、预热 1 秒、测量
5 秒、校准 9/32、计时采样步长 8，BPF/admission 开启，stats-only 和
timeslice extension 关闭。LiTL 构建为 `COND_VAR=1`。

入口顺序：direct、标准 LiTL、标准 LiTL、direct；每组先 MCS，再 MCS-TAS。
每个入口、每个后端两次，总共 8 次运行。

| 后端 | 直接调用 ops/s（均值） | 标准 LiTL ops/s（均值） | LiTL 相对差异 |
| --- | ---: | ---: | ---: |
| MCS | 1,295,507.01 | 1,288,468.66 | -0.54% |
| MCS-TAS | 1,393,887.44 | 1,332,622.87 | -4.40% |

各次使用同一个 benchmark 和同一组 direct 库，核对了实际加载映射：标准
LiTL 来自 `third_party/litl/lib`，没有加载 FlexGuard 的库。还检查了调度器
启用状态、enable_seq、退出后的清理，以及全部 192 个线程的进展和操作数求和。

这两次均值用于检查当前纯 mutex 工作负载的迁移结果，不能据此推断其他负载
的固定开销；用过条件变量等待的 mutex 仍承担 shadow 成本。
逐次数据见 [runs.csv](runs.csv)，汇总见 [summary.json](summary.json)，
二进制指纹见 [sha256.json](sha256.json)。原始日志、完整命令、源码修改对照
及迁移备份位于工作区 `target/litl-upstream-migration/` 和 CSV 中记录的运行目录。

迁移前的四轮优化结果仍保存在
[LiTL shadow 按需启用](../litl-lazy-shadow/README.md)，原始记录没有改写成
标准 LiTL 的测量。
