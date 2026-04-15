# Windowed Lock Stats 说明

## 范围

这份文档总结当前基于窗口的 `lock_stats` 实验，重点关注 `critical_ns` 和 `outside_ns` 的估计，主要包括：

- 基于时间窗口的热力图采样
- 最小窗口样本数门槛
- `EWMA(alpha=0.2)` 平滑
- 单次运行内与跨运行的稳定性
- 是否适合作为并发度控制依据

除非特别说明，下面的实验默认使用：

- `LB_SIMPLE_DISABLE_BPF=1`
- `LOCK_STATS_HEATMAP_SAMPLE_STRIDE=64`
- 基于时间窗口的 heatmap 导出
- 使用 unlock-gap 公式计算 `outside_ns`

## 当前导出内容

当前 [`src/lock_stats.rs`](/mnt/home/jz/lb_simple/.codex/worktree/handwrite/src/lock_stats.rs) 会为每个窗口导出 heatmap 行，包含：

- `window_sample_count`
- `window_valid`
- `window_avg_critical_ns_est`
- `window_avg_outside_ns_est`
- `window_avg_critical_ns_ewma`
- `window_avg_outside_ns_ewma`

其中 `EWMA` 使用固定的 `alpha=0.2`。

需要注意：当前 `window_avg_*_est` 不是由窗口内原始样本和直接计算出来的，而是通过 heatmap bin 的中点反推得到的。这使它更适合作为控制信号，而不是精确延迟估计。

## 最小窗口样本数门槛

为了避免 `1 ms` 窗口过于不稳定，heatmap 路径增加了最小样本数门槛：

- 环境变量：`LOCK_STATS_HEATMAP_MIN_WINDOW_SAMPLES`
- 当 `window_sample_count < N` 时，该窗口会被标记为 `window_valid=0`

这个门槛是有效的。在短时间运行里，它能过滤掉原本会显著干扰窗口决策的稀疏窗口。

## 单次运行内的稳定性

在 `128` 线程、`1 ms` 窗口、`min_window_samples=20` 的条件下，有效窗口比完整原始窗口集合稳定得多，但仍然不足以直接用原始窗口数值驱动每个 tick 的并发度调整。

观察到的现象：

- 主导 bin 往往停留在同一个粗粒度区间
- 有效窗口内部的分布较集中
- 在更高负载下，低样本窗口仍然频繁出现
- 原始窗口数值仍然会抖动，需要平滑

结论：

- 有效窗口能提升信号质量
- 仅靠有效窗口还不够
- 仍然需要平滑

## 跨运行稳定性

对 `128` 线程短跑重复多次后发现：

- 主导的粗粒度状态在不同 run 之间相对稳定
- 绝对数值在不同 run 之间并不稳定

观察到的模式：

- 主导 `(critical_bin, outside_bin)` 基本停留在同一粗区间
- `avg_outside_ns` 在不同 run 间波动很大
- 窗口切换率和有效窗口数量也有明显波动

结论：

- 系统在“粗粒度状态分类”这个层面是稳定的
- 在“绝对纳秒数值”这个层面并不稳定

这意味着窗口值更适合做控制状态推断，而不是精确延迟报告。

## EWMA 结果

对窗口序列做离线测试后发现，`EWMA(alpha=0.2)` 能明显降低窗口之间的抖动，而更小的 `alpha` 会进一步降低抖动。

之所以选择 `alpha=0.2`，是因为它是一个比较实用的折中：

- 比原始窗口平滑
- 比 `alpha=0.1` 或 `alpha=0.05` 的滞后更小

在测试中：

- `EWMA` 通常能把窗口步进变化压缩到原来的约 `2x` 到 `3x`
- 有些点改善更明显
- 有些坏点因为底层窗口序列太稀疏，仍然不可用

结论：

- `EWMA(alpha=0.2)` 是有用的
- 它能提升控制稳定性
- 但当有效窗口几乎不存在时，`EWMA` 无法凭空恢复信号质量

## 为什么窗口 EWMA 看起来更接近配置的 Outside 时间

在多组实验中，`window_avg_outside_ns_ewma` 看起来比全局 `avg_outside_ns` 更接近配置的 `--outside-ns`。

这是正常的，因为它们回答的是两个不同问题。

`avg_outside_ns` 反映的是：

- 所有 sampled unlock-gap 观测
- 队列和调度延迟
- 可能因为样本不足而未形成有效窗口的坏时段

`window_avg_outside_ns_ewma` 反映的是：

- 仅来自窗口化 heatmap 的样本
- 仅来自通过有效性门槛的窗口
- 一个经过分桶和 EWMA 平滑后的估计值

因此：

- `avg_outside_ns` 更接近整段运行的真实系统代价
- `window_avg_outside_ns_ewma` 更接近用于控制的“outside 工作量尺度估计”

对于并发度控制，后者通常更有用。

## 参数网格测试结论

曾运行以下参数网格：

- `threads = 32, 128`
- `critical_ns = 100, 300, 1000, 3000`
- `outside_ns = 0, 100, 300, 1000, 3000, 10000`

结果如下：

- `32` 线程：
  - 很多低到中等负载点都能产出可用的有效窗口
  - `EWMA` 经常能把 `outside` 的窗口步进抖动压低约 `2x`
  - 非常大的 `outside_ns` 仍然可能撞到 heatmap bin 上限，从而产生看似平稳但其实失真的平坦信号

- `128` 线程：
  - 在 `1 ms` 窗口下，很多点只有很少甚至完全没有有效窗口
  - 更高负载点无论是否做平滑都不可用
  - 瓶颈是样本密度不足，而不只是平滑质量不够

结论：

- 当前设置可以在中等负载区间支持基于窗口的控制
- 当前设置还不能在更重的 `128` 线程负载下支持可靠的逐窗口控制

## 面向 128 线程的窗口调优

为了改善 `128` 线程下的表现，比较了以下三组设置，并将 `LOCK_STATS_HEATMAP_MAX_BIN_NS=262144` 固定：

- `1 ms + min_samples=8`
- `2 ms + min_samples=12`
- `5 ms + min_samples=20`

代表性测试点包括：

- `critical=100, outside=100`
- `critical=100, outside=1000`
- `critical=300, outside=300`
- `critical=1000, outside=1000`

观察结果：

- `critical=100, outside=100`
  - 三组都能工作
  - `2 ms + 12` 在有效窗口数和稳定性之间的平衡最好

- `critical=100, outside=1000`
  - `2 ms + 12` 明显优于 `1 ms + 8`
  - `5 ms + 20` 也能工作，但没有明显更好

- `critical=300, outside=300`
  - `2 ms + 12` 仍然是三者里最有用的一组
  - `1 ms + 8` 和 `5 ms + 20` 可能看起来很平，因为估计值长时间停在同一个 bin 中点附近

- `critical=1000, outside=1000`
  - 三组都无法产出有效窗口

结论：

- 当前对 `128` 线程最实用的配置是 `2 ms + min_samples=12`
- 更重的点仍然处于不可用区间

## 已知限制

### 基于 Bin 中点反推

当前窗口平均值不是基于原始样本和，而是基于 heatmap bin 中点反推得到的。

这会带来：

- 量化误差
- 在 bin 中点附近出现平坦区域
- 与全局平均值脱钩

### Heatmap 上限饱和

如果 `LOCK_STATS_HEATMAP_MAX_BIN_NS` 太小，较大的 `outside_ns` 会饱和到最高 bin，从而产生“看起来很稳定但其实是截断”的输出。

因此这个上限必须足够大，至少能覆盖目标 workload。

### 有效窗口筛选带来的偏差

`window_valid` 会去掉稀疏窗口。这能提升控制质量，但也会让保留下来的序列偏向那些“样本更密”的时段。结果就是，保留下来的窗口 `EWMA` 可能会低估整段运行中真实出现过的 outside 延迟。

## 建议如何理解当前窗口 EWMA

当前窗口 `EWMA` 应该被当作：

- 控制信号
- 粗粒度状态估计
- 平滑后的 workload 强度代理量

不应该被当作：

- 精确延迟指标
- 全局 `avg_outside_ns` 的直接替代品

## 建议的下一步改动

下一步实现不应该再通过 heatmap bin 反推窗口均值。

更合理的做法是直接在窗口内保留原始和：

- `window_critical_ns_sum`
- `window_outside_ns_sum`
- `window_sample_count`

然后直接导出：

- `window_avg_critical_ns`
- `window_avg_outside_ns`
- `window_avg_critical_ns_ewma`
- `window_avg_outside_ns_ewma`

这样既能保留当前以控制为导向的设计，也能让导出的窗口数值更接近真实采样结果。
