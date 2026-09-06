# simplify：fullhook 差异归因与选择性迁入

本轮先切换到 `simplify@8fc18994aa51f27b91bf6b490a575a309add8da3`，使用独立源码快照做单项及组合消融；fullhook 对照固定为 `4e5998e21c458e9b162855ef3c3d5c7e42b42ebb`。没有整批迁入 fullhook 或条件变量设计。

## 保留范围

本轮保留的生产改动只有 BPF `accordin_tick`：当已获 admission 的 WAITING/SPINNING 线程运行，且 NORMAL_DSQ 有排队任务时，将当前时间片设为 0，让普通任务获得调度机会；保留原 admission 名额。两个后端均完成三轮 LevelDB 确认、完整 streamcluster、barrier 和实际工作区正确性检查。

普通锁布局、TAS 等待循环、LiTL 入口、relock epoch 复用、signal/broadcast 接力、取消/超时语义维持 simplify。没有增加 shadow mutex、COND_VAR 开关或每锁 DSQ。

## 迁入决策

下表的筛选与组合数据均为 MCS-TAS；单次探索结果只用于筛选，样本数与后续复测见详细表。唯一迁入项另外经过两个后端的三轮确认和应用回归。

| 差异/控制 | 观察 | 决定 |
| --- | --- | --- |
| tick：已获准自旋者为普通队列提供运行机会 | 最终 fillrandom 达到基线的 2.879×，readrandom 持平 | 迁入 |
| holder：持锁线程 local 入队并抢占 | fillrandom 筛选约 14 Kops/s，基线约 58 | 不迁入 |
| self：yield 时占用当前 CPU 的空 admission 名额 | 筛选没有明显收益 | 不迁入 |
| idle：select_cpu 直接投递空闲 CPU | 单项写入有改善，但叠加 tick 后从约 161 降到 99 Kops/s | 不迁入 |
| enqlast：SCX_OPS_ENQ_LAST | 两次读对照平均回退 8.5% | 本轮不迁入；该标志的最后 runnable task 进展用途不能仅凭性能否定 |
| bpf_all：整组 BPF 策略 | 筛选读写均弱于单独 tick；与 TTAS 组合读回退约 25% | 不整批迁入 |
| TTAS / 紧凑锁布局 | 单项与组合效果不同；反向 TTAS 未恢复 fullhook 读性能 | 保留 simplify 的算法与布局 |
| initial-exec TLS | 筛选收益小，通用 direct dlopen 明确失败 | 不迁入 |
| fullhook condvar 自旋 | 在 fullhook 内对写入有较大帮助；simplify + tick 已达到更高吞吐量 | 本轮无需迁入 |
| signal 独立唤醒 | 单独或叠加 tick 的写入均严重回退 | 保留原接力 |

## 配置与方法

- 与[分支报告](../leveldb-branches-20260906/README.md)相同的 LevelDB 1.20 二进制、100 万键种子、192 worker、CPU 0–95、30 秒、100 B value、tmpfs；BPF/admission 开启，统计关闭。每次 readrandom 复制并核对种子，fillrandom 使用全新路径。
- 使用 BENCH_TOTAL 的总操作数 / 合并墙钟区间，不能取逐线程 micros/op 的倒数。固定二进制 SHA256 为 `5f4ee4e128e60af6ff13a434d82c39bfee52ff80dc10d815f5a6f60b93034171`。
- 本机 sysfs 报告 L1/L2 coherency_line_size 为 64 B，L3 为 128 B，见[缓存拓扑](cache-topology.json)；64 B 对齐不等于在所有缓存层完全隔离。
- 各候选源码由 git archive 导出到独立目录，记录相对固定提交的 patch、源码和动态库哈希。每次运行核对实际加载库、BPF fd、sched_ext 状态和 enable_seq；性能测试与编译串行，使用现有 benchmark flock。
- screen 是探索性单次候选测试，每四个候选夹测 baseline；combo-screen 用于排除组合回退。最终吞吐量只使用 confirm 的三轮同场比较，不将探索样本并入确认均值。reverse-ttas 和 storage-read 是在 fullhook 上的反向/存储对照。
- 失败与超时保留，不能只挑成功值计算完整对比。各阶段原始记录见对应子目录；CSV 只便于查看，JSONL 保存完整审核信息。

## LevelDB 确认结果

单位 Kops/s，三次均值；当前/基线仅在两组均有三次有效结果时给出。

| 工作负载 | 锁 | simplify 基线 | 仅 tick | 当前/基线 | 有效次数 |
| --- | --- | ---: | ---: | ---: | --- |
| readrandom | mcs_accordin | 584.799 | 587.489 | 1.005× | 3/3、3/3 |
| readrandom | mcs_tas_accordin | 935.436 | 938.344 | 1.003× | 3/3、3/3 |
| fillrandom | mcs_accordin | 51.652 | 79.639 | 1.542× | 3/3、3/3 |
| fillrandom | mcs_tas_accordin | 56.955 | 163.958 | 2.879× | 3/3、3/3 |

### 确认测量逐轮与波动

| 工作负载 | 锁 | 版本 | 三次 Kops/s | CV |
| --- | --- | --- | --- | ---: |
| readrandom | mcs_accordin | baseline | 586.589 / 573.763 / 594.046 | 1.75% |
| readrandom | mcs_accordin | tick | 596.862 / 593.739 / 571.865 | 2.32% |
| readrandom | mcs_tas_accordin | baseline | 933.774 / 932.664 / 939.869 | 0.41% |
| readrandom | mcs_tas_accordin | tick | 937.612 / 935.087 / 942.333 | 0.39% |
| fillrandom | mcs_accordin | baseline | 47.871 / 54.475 / 52.610 | 6.59% |
| fillrandom | mcs_accordin | tick | 80.483 / 76.350 / 82.084 | 3.71% |
| fillrandom | mcs_tas_accordin | baseline | 59.130 / 55.708 / 56.027 | 3.32% |
| fillrandom | mcs_tas_accordin | tick | 159.632 / 170.961 / 161.281 | 3.73% |

## 完整 streamcluster 与 barrier 回归

streamcluster 参数为 `10 30 512 32768 32768 2000 none <output> 192`；时间使用原程序计数 / 100,000,000。每次输出必须匹配 SHA256 `dfeea2357203cceeb8bcdac4984ffc9da9c953f1f1d19c06990626a4575ef01f`。barrier 为 192 线程、100 轮。单位秒，越低越好。

| 工作负载 | 锁 | simplify 基线 | 仅 tick | 基线/当前 | 有效次数 |
| --- | --- | ---: | ---: | ---: | --- |
| stream | mcs_accordin | 73.532 | 71.307 | 1.031× | 3/3、3/3 |
| stream | mcs_tas_accordin | 74.254 | 73.527 | 1.010× | 3/3、3/3 |
| barrier | mcs_accordin | 0.298 | 0.297 | 1.003× | 3/3、3/3 |
| barrier | mcs_tas_accordin | 0.307 | 0.291 | 1.056× | 3/3、3/3 |

## 归因证据

screen 中的 `compact` 只将外部 raw lock 的 tail/locked 改为 8 字节对齐，不把锁内嵌到应用对象；它不能独自检验与应用字段共享缓存行的影响。`tls` 仅改变 direct 库的 TLS 模型；`bpf_all` 迁入 fullhook 的 BPF 策略但去除 USER_CV 过滤，保持 simplify 的 word 编码。`signal` 是从设计方案导出的 signal-only 独立唤醒控制，保留 broadcast 接力，不是完整 fullhook condvar。

fullhook0/fullhook1000 是同一动态库、仅自旋预算不同。fullhook_tas 只将 fullhook 的队首 TTAS 改回反复 exchange；其余策略不变。fullhook_heap 将 raw lock 移到独立的 64 字节对齐分配，tail/locked 仍紧凑；fullhook_heap+padding 再将两字段分开对齐。这些存储控制同时包含必要的指针间接访问、冷路径分配和潜在的 NUMA first-touch 差异，不能将结果全部归为缓存行共享或声称仅改变了一条 cache 指令。

### screen

| 工作负载 | 版本 | 有效/尝试 | 均值 Kops/s | 逐次 Kops/s |
| --- | --- | ---: | ---: | --- |
| readrandom | baseline | 4/4 | 921.023 | 915.506 / 938.705 / 931.501 / 898.382 |
| readrandom | compact | 1/1 | 930.802 | 930.802 |
| readrandom | ttas | 1/1 | 807.256 | 807.256 |
| readrandom | tls | 1/1 | 962.133 | 962.133 |
| readrandom | holder | 1/1 | 944.010 | 944.010 |
| readrandom | tick | 1/1 | 944.715 | 944.715 |
| readrandom | self | 1/1 | 937.758 | 937.758 |
| readrandom | idle | 1/1 | 942.053 | 942.053 |
| readrandom | enqlast | 1/1 | 859.567 | 859.567 |
| readrandom | bpf_all | 1/1 | 835.743 | 835.743 |
| readrandom | signal | 1/1 | 928.739 | 928.739 |
| readrandom | fullhook0 | 1/1 | 637.798 | 637.798 |
| readrandom | fullhook1000 | 1/1 | 652.816 | 652.816 |
| fillrandom | baseline | 4/4 | 57.640 | 55.164 / 58.919 / 58.524 / 57.953 |
| fillrandom | compact | 1/1 | 53.908 | 53.908 |
| fillrandom | ttas | 1/1 | 57.184 | 57.184 |
| fillrandom | tls | 1/1 | 53.519 | 53.519 |
| fillrandom | holder | 1/1 | 14.169 | 14.169 |
| fillrandom | tick | 1/1 | 164.504 | 164.504 |
| fillrandom | self | 1/1 | 59.123 | 59.123 |
| fillrandom | idle | 1/1 | 75.232 | 75.232 |
| fillrandom | enqlast | 1/1 | 63.694 | 63.694 |
| fillrandom | bpf_all | 1/1 | 70.549 | 70.549 |
| fillrandom | signal | 1/1 | 17.286 | 17.286 |
| fillrandom | fullhook0 | 1/1 | 23.965 | 23.965 |
| fillrandom | fullhook1000 | 1/1 | 145.124 | 145.124 |

### combo-screen

| 工作负载 | 版本 | 有效/尝试 | 均值 Kops/s | 逐次 Kops/s |
| --- | --- | ---: | ---: | --- |
| fillrandom | baseline | 1/1 | 57.211 | 57.211 |
| fillrandom | tick | 1/1 | 160.816 | 160.816 |
| fillrandom | tick+idle | 1/1 | 98.632 | 98.632 |
| fillrandom | tick+signal | 1/1 | 18.079 | 18.079 |

### reverse-ttas

| 工作负载 | 版本 | 有效/尝试 | 均值 Kops/s | 逐次 Kops/s |
| --- | --- | ---: | ---: | --- |
| readrandom | fullhook1000 | 3/3 | 657.510 | 648.055 / 661.983 / 662.490 |
| readrandom | fullhook_tas | 3/3 | 662.922 | 686.645 / 657.280 / 644.842 |

### read-interactions

| 工作负载 | 版本 | 有效/尝试 | 均值 Kops/s | 逐次 Kops/s |
| --- | --- | ---: | ---: | --- |
| readrandom | baseline | 2/2 | 937.988 | 947.419 / 928.558 |
| readrandom | enqlast | 2/2 | 858.648 | 871.269 / 846.027 |
| readrandom | compact+ttas | 2/2 | 777.272 | 757.541 / 797.002 |
| readrandom | ttas+bpf_all | 2/2 | 701.155 | 702.435 / 699.876 |

### storage-read

| 工作负载 | 版本 | 有效/尝试 | 均值 Kops/s | 逐次 Kops/s |
| --- | --- | ---: | ---: | --- |
| readrandom | fullhook1000 | 3/3 | 648.736 | 649.105 / 647.134 / 649.968 |
| readrandom | fullhook_heap | 3/3 | 702.631 | 704.171 / 693.708 / 710.015 |
| readrandom | fullhook_heap+padding | 3/3 | 721.480 | 720.176 / 729.820 / 714.442 |

### 结论边界

单项筛选中 tick 的写入收益最大；叠加 idle 直接投递或 signal 独立唤醒反而回退，因此不合入这些组合。signal-only 的负结果也说明，之前“已完成 writer 的 FIFO 可能拖住下一批 leader”的源码推断不足以指导迁入；不能忽略取消接力所增加的竞争。

fullhook 的自旋开/关差异证明自旋对该分支的写入路径有显著影响，但不意味着 simplify 必须引入同一套 condvar 自旋才能达到其吞吐量。普通任务能否获得 CPU 是独立因素。LevelDB 的批量写入 leader 在锁外写日志/更新 memtable，后台压缩也需要普通任务的执行机会；tick 收益与改善这些任务的进展相符。本轮没有线程级剖析来分摊 leader 与后台压缩的贡献。

反向 TTAS 的三次 fullhook 对照没有显示能恢复整个读性能差距，不能从 simplify 单次 TTAS 回退直接推导 fullhook 的全部损失。存储控制和组合控制用于进一步分辨交互；差异百分比不能直接相加。

三轮存储对照中，fullhook 从内嵌锁的约 649 Kops/s 提升到外部紧凑锁的约 703 Kops/s，分开对齐后约 721 Kops/s。这支持存储方式解释部分读性能差距，但尚未完全分解整套分支的差距，也没有单独量化缓存共享与 NUMA 放置的贡献。

## 正确性与保留记录

候选的构建、直接 API BPF 检查、涉及 condvar 变化时的 LiTL 完整检查均保留在日志归档。TLS 变体的通用 direct dlopen 检查报 `cannot allocate memory in static TLS block`；它仅作为已通过 preload 检查的诊断控制，未迁入通用 direct 库。fullhook 原有 MCS 超时未在本任务中宣称修复，fullhook 性能消融只使用 MCS-TAS。

最终工作区检查与生产 diff 见 [工作区验证](integration.md) 和 [最终补丁](production.patch)。生产改动选择与探索性 patch 必须分开阅读；目录中的候选 patch 不代表已经合入。

## 原始材料与复现

- [结果 CSV](results.csv)、各阶段的 results.jsonl / metadata.json / summary.json。
- [原始日志与 streamcluster 输出](logs.tar.gz)、[候选源码/库哈希清单](variant-metadata.tar.gz)。
- `build_variants.py`、`run.py`、`run_extended.py`、`preflight.py`、`regression.py` 和每个候选的 `.patch`。原始构建与库保留在 `target/leveldb-attribution-20260906/variants/`。

这些归档脚本按本次 target 目录位置编写。复现时将脚本复制到 `target/leveldb-attribution-20260906/` 再运行，需相同 LevelDB 二进制、种子及前述 fullhook 固定 worktree；保留分支报告目录，runner 会复用其中的 `run.py`。已有候选目录不会被 build_variants.py 覆盖；使用干净 checkout 准备新候选。性能 runner 支持新的 stage 名称，原 stage 仅允许相同配置续跑，不重跑失败记录。

```sh
python3 target/leveldb-attribution-20260906/build_variants.py baseline tick
sudo python3 target/leveldb-attribution-20260906/preflight.py baseline tick
sudo python3 target/leveldb-attribution-20260906/run.py --stage repeat-confirm \
  --arms baseline tick --backends mcs_accordin mcs_tas_accordin --repeats 3
sudo python3 target/leveldb-attribution-20260906/regression.py --stage repeat-regression \
  --arms baseline tick --repeats 3
```

报告生成时间：2026-09-06T04:28:44.638144+00:00。归档尝试 107 次，有效 107 次；确认与回归矩阵完整有效：True。
