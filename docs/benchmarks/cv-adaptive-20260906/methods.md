# simplify：迁移 fullhook 的自适应条件等待

来源为 [fullhook-admission 的 f7fedc9](https://github.com/JBPFM/accordin/commit/f7fedc94c46ff98f3ad20ada1fecc414b3fe233a)，基线为 `simplify@934bd1c`。本轮从远端获取该提交，并将本地 fullhook-admission 快进到该版本；工作区始终在 simplify 上实现迁移。

## 实现与迁移边界

- BPF 发布 NORMAL_DSQ 与 WAITING_DSQ 是否有排队任务的提示，只在提示值改变时写入。消费者只需判断是否为零，无需每次 enqueue/dispatch 都写全局队列长度；该提示仍是近似信息，不参与锁正确性的判定。CV 等待者尝试一次 admission，未获准时仍能通过普通队列恢复运行并停泊。只有 CV 请求可以在这次 yield 中借用当前 CPU 的空名额；普通锁的续用/排队规则不变。
- 每个 condvar 的失败评分范围为 0–8；获准自旋未等到通知加 2，等到通知减 1。评分达到 3 时，排队需求会阻止或结束自旋。任何评分下，预算耗尽或失去 admission 名额都会结束自旋。被抢占的 CV 自旋者归还名额，已进入 raw lock 队列的线程继续保留名额。
- CV 自旋、通知发布和 relock 使用同一 epoch。通知与自旋退出的更新使用 CAS/发布协议，退出不能覆盖通知已发布的 WAITING。
- 自旋观察逻辑通知，重获锁仍遵循 simplify 的 mutex 接力队列。被通知但尚未获得接力权的线程停泊，不占用 CPU 等接力。signal/broadcast、取消补偿、超时后重获锁、嵌套等待均保留。
- waiter 的 parked 发布与 wake 发布使用顺序一致性握手：等待者先看到 wake，或者通知者看到 parked 并执行 futex wake。正在自旋的接力者可以省去唤醒 syscall。没有 shadow mutex，也没有 COND_VAR/COND_VAE 开关。
- 保留 simplify 的 raw lock 算法、外部锁存储和普通调度策略；未迁入 fullhook 的 holder 抢占、idle 直接投递、普通等待者 self-admit、TTAS/内嵌锁或 initial-exec TLS。

## 默认预算：MCS 为 0，MCS-TAS 为 50 µs

直接使用 fullhook 的 1000 µs 默认预算，在 MCS-TAS 的 LiTL 接力路径上造成明显 fillrandom 回退。关闭自旋后吞吐量恢复，50 µs 控制则优于关闭自旋；逻辑通知与接力分离也不能单独消除长预算的损失。因此保留队列需求与历史评分机制，为 MCS-TAS 选择 50 µs，并通过独立的三轮确认检查其取舍。

本次 LevelDB 源码的 `DBImpl::Write` 每次在栈上构造包含独立 condvar 的 Writer；这一点限制了每对象历史评分的复用，长预算的首次尝试成本仍可能反复发生。写入 leader 也需要在 mutex 外写日志和更新 memtable。结合预算对照，自旋占用 admission/CPU 是性能变化的合理解释；本轮没有新增线程级 profiling，不将其表述为已精确分摊的热点。

在同为 50 µs 的控制中，禁用“失败历史 + 排队需求”判断的 `balanced-fixed` 跑 streamcluster 为 100.90 秒，启用判断的 `balanced` 三轮均值为 78.57 秒。这个单次消融支持保留历史机制，但不足以精确估计其收益大小。

末尾复查时，MCS 的 fillrandom 基线从早期三轮均值 69.6 Kops/s 上升到 75.9 Kops/s，已与 50 µs 候选的 75.6 Kops/s 相当；同时 MCS 的 streamcluster 有明显耗时成本。因此不将这个不稳定的写入收益作为默认启用的理由，MCS 默认使用 0 µs，另行三轮检查；机制仍保留，可显式设置非零预算启用。

MCS-TAS 的三轮 fillrandom 为 187.9 Kops/s，最后新构建复查为 181.6 Kops/s，均高于早期基线三轮均值 159.9 Kops/s 和末尾单次 156.4 Kops/s。streamcluster 则仍有代价：相同 50 µs 路径三轮为 76.79 秒，最后新构建单次为 83.56 秒，而基线三轮为 75.34 秒、末尾单次为 72.42 秒。最后这一较慢样本完整保留；目前不能宣称 streamcluster 无回退，也不能将约 2% 的三轮差异当作稳定上界。

`ACCORDIN_CV_SPIN_US` 接受 0–1000000，MCS 默认 0、MCS-TAS 默认 50；0 只关闭自旋。无 BPF、禁用 admission、stats-only 或嵌套等待时不借用 CV 自旋名额。不同机器和不同等待分布可能需要不同预算。

## 配置与审计

- 192 worker、CPU 0–95、BPF/admission 启用、统计关闭；性能运行和编译使用 `/tmp/mutexbench-sweep-multi-lock.lock` 串行。
- LevelDB 使用与前次归因相同的二进制 SHA256 `5f4ee4e128e60af6ff13a434d82c39bfee52ff80dc10d815f5a6f60b93034171`，100 万键种子、100 B value、30 秒、tmpfs。每次读复制并核对种子，每次写使用新数据库。吞吐量取 BENCH_TOTAL 总操作数 / 合并墙钟时间。
- 完整 streamcluster 参数：`10 30 512 32768 32768 2000 none <output> 192`。程序计时除以 100000000 得秒；每次输出核对 SHA256 `dfeea2357203cceeb8bcdac4984ffc9da9c953f1f1d19c06990626a4575ef01f`。
- 每次核对实际加载库、BPF fd、sched_ext 状态和 enable_seq。baseline、balanced、pressure、defaults 和控制版本都是独立源码/构建，元数据记录源码及动态库哈希。工作区生产源码与 defaults 快照一致，并另外完成工作区重建检查。defaults 相对 pressure 只调整 MCS 的默认预算为 0，TAS 的 50 µs 行为不变。
- 表中的预算是 runner 传入的环境参数；baseline 没有这个参数的解析或 CV 自旋实现，始终使用原停泊路径。应用测试显式传入所选预算，正确性检查还覆盖未设置预算变量的后端默认值。继承的 runner 中有旧 fullhook arm 入口，本次没有运行该入口；来源机制以归档的 f7fedc9 补丁为准。
- 探索是单次控制。第一阶段 baseline 与 balanced 交错各测三轮；将需求提示改为布尔值并减少共享写入后，pressure 在后续阶段独立各测三轮，再在末尾补测基线各一次检查时间漂移。最终 MCS 默认 0 µs 使用新构建三轮数据；TAS 使用 pressure 相同 50 µs 路径的三轮数据，并对新构建再复查一次。主表不混入探索或末尾复查值。因此最终版本与基线不是逐轮配对测量，几个百分点的差异应结合逐轮波动及末尾基线复查看待。原始数据保留失败标记，不丢弃回退样本。

## 正确性检查

以下命令在最终工作区全部通过：

```sh
make -j8 check check-litl
sudo make check-bpf check-litl-bpf
sudo taskset -c 0,1 env LITL_TEST_THREADS=24 LITL_TEST_ITERATIONS=1000 \
  make check-bpf check-litl-bpf
sudo env ACCORDIN_CV_SPIN_US=50 make check-bpf check-litl-bpf
```

覆盖直接 ABI、C/C++ LiTL、首次 condvar 等待、signal/broadcast、取消补偿、超时和 clockwait、多个 CV 共用 mutex、嵌套等待及 NDEBUG 无 shadow mutex。新增 `check-spin` 对真实 direct 自旋函数注入确定性调度事件，检查排队需求/评分、失去名额、通知与退出竞争、逻辑通知先于接力、同 epoch 重获锁、过期 deadline 与嵌套跳过。公开头文件也通过严格 C11 语法检查。

## 原始材料与复现

- `production.patch` 是迁入补丁；其余候选 patch 仅是归因材料。`upstream-commit.txt`/`upstream.patch` 保存来源。
- `results.csv`、`summary.json` 和各阶段 JSON 保存结果；`logs.tar.gz` 保存应用日志、streamcluster 输出、构建和检查日志；`variant-metadata.tar.gz` 保存独立候选的源码/库哈希。
- runner 复用仓库已有的 `docs/benchmarks/leveldb-branches-20260906/run.py`。将本目录 Python 脚本复制到 `target/cv-adaptive-20260906/` 后运行。已有候选和阶段不应覆盖；需要相同的应用二进制与种子。`build.py` 拒绝覆盖已有候选。

```sh
python3 target/cv-adaptive-20260906/build.py baseline
python3 target/cv-adaptive-20260906/build.py defaults \
  --patch docs/benchmarks/cv-adaptive-20260906/production.patch
sudo python3 target/cv-adaptive-20260906/budget.py --stage repeat-mcs-leveldb \
  --arms baseline defaults --backends mcs_accordin --spin-us 0 --repeats 3
sudo python3 target/cv-adaptive-20260906/budget.py --stage repeat-tas-leveldb \
  --arms baseline defaults --backends mcs_tas_accordin --spin-us 50 --repeats 3
sudo python3 target/cv-adaptive-20260906/regression_budget.py --stage repeat-mcs-stream \
  --arms baseline defaults --backends mcs_accordin --modes stream --spin-us 0 --repeats 3
sudo python3 target/cv-adaptive-20260906/regression_budget.py --stage repeat-tas-stream \
  --arms baseline defaults --backends mcs_tas_accordin --modes stream --spin-us 50 --repeats 3
```
