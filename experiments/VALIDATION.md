# 实际构建与试运行

## LiTL 统一前端后的 smoke

机器：Intel Xeon Gold 5318Y，2 socket × 24 核、无 SMT；CPU 0–47，P=48，
线程数 12/24/48/96/192。六种锁全部经 LiTL `directlock`/`directcond` 前端接入。

```sh
python3 experiments/prepare.py --jobs 12 --build target/experiments-litl
sudo -n python3 experiments/run.py --profile smoke --build target/experiments-litl \
  --out target/results/litl-smoke-20260909
```

180 个配置全部尝试：**136 个有效样本、14 次超时、30 个 unsupported、0 个 error/invalid**。
运行前的绑定检查：mcs/mcs-tas/gcr 无 BPF、sched_ext enable_seq 不变；flexguard 观察到
BPF fd、enable_seq 不变；accordin 观察到 direct 库、BPF fd 且 enable_seq 恰好加一。
mcs-tse 由 `tse_probe` 判定内核不提供 rseq slice extension，30 个配置全部记为
`unsupported`。退出时 sched_ext=disabled，所有被测文件哈希不变。

| 锁 | ok | timeout |
|---|---|---|
| mcs | 23 | 7 |
| mcs-tas | 28 | 2 |
| gcr | 27 | 3 |
| flexguard | 28 | 2 |
| mcs-tse | 0 | 0（30 unsupported） |
| accordin | 30 | 0 |

超时集中在 Streamcluster 和 Raytrace 的 96/192 线程，属于 30 s smoke 预算内未完成，
不是死锁判定；短样本不作为性能排名。

## 2026-09-09 更早的一轮（旧构建路径）

机器：TaiShan-v110，96 物理核、无 SMT；CPU 0–95，线程数 24/48/96/192/384。
构建使用当前工作区（包含运行前已有的未提交 Accordin 修改）；精确版本、源文件与动态库
哈希保存在每个结果目录的 `manifest.json`，不能只用 Git HEAD 替代这份源码记录。

## 完整五档 smoke

```sh
sudo -n python3 experiments/run.py --profile smoke --timeout 20 \
  --out results/overload-smoke-20260909
```

180 个配置全部尝试：**133 个有效样本、17 次超时、30 个 unsupported、0 个 error/invalid**。
六个负载、五个支持的锁、五档线程均实际执行。MCS-TSE 保留全部 30 个配置点，未以其他
实现代替。内核/用户态缺少 rseq slice-extension 支持；独立 `PR_RSEQ_SLICE_EXTENSION_GET`
检查也返回 `EINVAL`。

运行前五个支持后端都通过真实符号绑定、8 线程共享计数、嵌套 mutex、条件变量唤醒检查。
所有有效应用样本通过计时/工作量/相应输出检查；Accordin 的 BPF 和 sched_ext 加载得到
实际观测。退出时 sched_ext=disabled，所有被测文件哈希不变。

| workload | 20 秒上限内未完成的配置 |
|---|---|
| LevelDB readrandom | MCS 192 |
| LevelDB fillrandom | MCS 384、MCS-TAS 384 |
| Streamcluster | MCS 96/192/384；MCS-TAS 192/384；GCR 96/192/384；FlexGuard 192/384；Accordin 384 |
| Raytrace | MCS 96/192/384 |
| CacheDB | 无 |
| RocksDB | 无 |

这里的超时不是死锁判定。尤其 Streamcluster 的 smoke 缩小了点数与维度，却保留了大量
barrier，同步占比与正式配置不同。Accordin 在这个 4P 配置仍然超时，不能声称所有应用都
已消除性能崩溃。短样本的启动、调度和缓存扰动也较大，不将这些单样本作为正式排名。

后续已对 Accordin/Streamcluster 384 线程超时做性能采样、阶段计数和重复复现：
原二进制三次均完成，ROI 为 45.281/41.028/28.882 秒，仍全部超过原 20 秒预算。
主要开销来自近万轮两阶段 barrier 的约 746 万次等待及广播释放。
两个 BPF 修改和配置对照均未验证出能消除超时的修复，策略修改已撤回。
完整证据和复现入口见 [Streamcluster 超时定位](STREAMCLUSTER_TIMEOUT.md)。

本地原始证据：
[配置](../results/overload-smoke-20260909/config.json)、
[逐次记录](../results/overload-smoke-20260909/results.jsonl)、
[CSV 汇总](../results/overload-smoke-20260909/summary.csv)、
[运行前校验](../results/overload-smoke-20260909/preflight.json)、
[最终审计](../results/overload-smoke-20260909/audit.json)、
[吞吐量图](../results/overload-smoke-20260909/throughput.png)、
[相对 P 的保留比例](../results/overload-smoke-20260909/retention.png)。

## 正式工作量的补充试跑

进一步运行默认 full 数据库工作量，比较 P=96 和 4P=384 的 MCS/Accordin，每点一次、
无额外预热轮、每进程 45 秒上限；每个随机读进程内仍执行 cache 预热。

```sh
sudo -n python3 experiments/run.py --profile full --threads 96 384 --locks mcs accordin \
  --workloads leveldb-readrandom leveldb-fillrandom kyoto-cachedb rocksdb raytrace \
  --repeats 1 --warmups 0 --timeout 45 --out results/overload-full-pilot-20260909
```

**15 个有效样本、5 次超时、0 个 error/invalid**。下表为应用 ROI 秒数，越低越好。
“超时”是整个进程达到 45 秒预算，不是假设 ROI 恰好为 45 秒，也不转换成零吞吐量。

| workload | MCS 96 | Accordin 96 | MCS 384 | Accordin 384 |
|---|---:|---:|---|---:|
| LevelDB readrandom，1,966,080 reads | 31.824 | 2.271 | 超时 | 2.261 |
| LevelDB fillrandom，983,040 puts | 14.839 | 7.469 | 超时 | 8.467 |
| CacheDB，1,966,080 ops | 1.790 | 0.989 | 超时 | 1.001 |
| RocksDB readrandom，1,966,080 reads | 31.339 | 3.477 | 超时 | 3.519 |
| Raytrace，FlexGuard 原配置 | 3.449 | 0.592 | 超时 | 0.621 |

这些单次结果支持继续用此参数做正式重复实验：Accordin 在这五项的 4P 点保有有效进度，
MCS 未在预算内完成。但这不是其他 baseline 的 full 比较，不构成普遍优越性或统计显著性
证明，也没有独立测得每个实际临界区的持有时间分布。观察器和后台系统活动未完全隔离。

另外，以 Accordin、24 个线程执行 **FlexGuard 原始 Streamcluster 参数**：

```sh
sudo -n python3 experiments/run.py --profile full --threads 24 --locks accordin \
  --workloads streamcluster --repeats 1 --warmups 0 --timeout 180 \
  --out results/streamcluster-full-pilot-20260909
```

完整运行成功，ROI **23.999 秒**；输出维度 512、聚类权重总和 32768，通过输出校验。
这验证正式参数可以运行；尚未完成 Streamcluster full 工作量的全部锁/线程矩阵。

两轮补充试跑结束时也均确认文件哈希不变、sched_ext=disabled。本轮最终验证共记录
**201 个配置尝试：149 个有效样本、22 次超时、30 个 unsupported、0 个 error/invalid**，
不包含前置定位问题的 pilot 和独立锁校验。

证据：[full 逐次记录](../results/overload-full-pilot-20260909/results.jsonl)、
[full 汇总](../results/overload-full-pilot-20260909/summary.csv)、
[full 审计](../results/overload-full-pilot-20260909/audit.json)、
[Streamcluster 记录](../results/streamcluster-full-pilot-20260909/results.jsonl)、
[Streamcluster 审计](../results/streamcluster-full-pilot-20260909/audit.json)。

## 兼容性问题的定位

前置 pilot 的失败记录保留在 `results/overload-pilot*-20260909/`，未混入最终 smoke：

- `taskset` 继承 LD_PRELOAD 会使 Accordin 在 exec 前后加载两次。最终脚本通过
  `taskset ... env LD_PRELOAD=... application` 将注入限定到应用，检查 enable_seq +1。
- FlexGuard 系列 timedwait 会直接 `exit(1)`。RocksDB writable/CompactedDB 的后台定时
  线程触发该分支，退出过程中主线程可见已释放的参数字符串，表现为乱码 benchmark 名称。
  GDB 抓到 `Timedwait not supported yet.`；最终采用所有锁统一的
  `readonly=1, open_files=1000` 普通只读缓存测试，不把兼容性退出算作锁崩溃。
- 旧归档 ARM patch 有两处缺失换行标记，已修复新副本；新脚本使用私有 LiTL/应用/锁构建
  目录，避免覆盖旧的 root 构建文件。

`python3 -m unittest discover -s experiments -p 'test_*.py' -v` 的五个检查通过，涵盖
聚合吞吐量及固定操作数、超时不能记成零、CPU 列表解析、RLE 截断和进程超时回收。
