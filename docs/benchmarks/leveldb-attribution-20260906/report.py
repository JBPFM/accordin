#!/usr/bin/env python3
"""Archive measured attribution evidence without mixing exploratory/final runs."""
import csv,datetime,json,shutil,statistics,tarfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2]
WORK=Path(__file__).resolve().parent
OUT=ROOT/'docs/benchmarks/leveldb-attribution-20260906'
STAGES=['screen','combo-screen','reverse-ttas','read-interactions','confirm','regression','storage-read']
def load(stage):
 p=WORK/stage/'results.jsonl'
 return [json.loads(l) for l in p.read_text().splitlines()] if p.exists() else []
def group(rows,mode,backend,name):
 return [s for s in rows if s['benchmark']==mode and s['backend']==backend and s.get('branch',s.get('variant'))==name]
def values(rows,key):return [s[key] for s in rows if s['valid']]
def mean(rows,key):
 vals=values(rows,key);return statistics.mean(vals) if vals else None
def fmt(value,scale=1):return '—' if value is None else f'{value/scale:.3f}'
def main():
 OUT.mkdir(parents=True,exist_ok=True)
 allrows=[]
 for stage in STAGES:
  rows=load(stage)
  if not rows:continue
  dest=OUT/stage;dest.mkdir(exist_ok=True)
  for filename in ['results.jsonl','metadata.json','summary.json']:
   p=WORK/stage/filename
   if p.exists():shutil.copy2(p,dest/filename)
  allrows.extend(dict(s,stage=stage) for s in rows)
 with (OUT/'results.csv').open('w',newline='') as f:
  writer=csv.DictWriter(f,fieldnames=['stage','id','benchmark','variant','backend','repeat','valid','timeout','ops_per_second','seconds'],lineterminator='\n');writer.writeheader()
  for s in allrows:writer.writerow({k:s.get(k,s.get('branch') if k=='variant' else '') for k in writer.fieldnames})
 for p in WORK.glob('*.py'):shutil.copy2(p,OUT/p.name)
 for p in WORK.glob('*.patch'):shutil.copy2(p,OUT/p.name)
 with tarfile.open(OUT/'logs.tar.gz','w:gz') as tf:
  for p in sorted(WORK.rglob('*')):
   rel=p.relative_to(WORK)
   if rel.parts[0] in ('variants','preserved','__pycache__'):continue
   if p.is_file() and (p.suffix in ('.log','.clusters','.csv') or p.name.startswith('check-')):tf.add(p,arcname=str(rel))
 with tarfile.open(OUT/'variant-metadata.tar.gz','w:gz') as tf:
  for p in sorted(WORK.glob('*.json')):tf.add(p,arcname=p.name)
 if (WORK/'integration-metadata.json').exists():
  meta=json.loads((WORK/'integration-metadata.json').read_text())
  shutil.copy2(WORK/'integration-metadata.json',OUT/'integration-metadata.json')
  passed=meta.get('source_unchanged',False) and len(meta['commands'])==3 and all(c['returncode']==0 for c in meta['commands'])
  detail='# 工作区合入验证\n\n'
  detail+=f'工作区分支：simplify；检查完整通过：{passed}。生产改动仅为 src/bpf/main.bpf.c 的 accordin_tick；见 [最终补丁](production.patch)。\n\n'
  detail+=f'确认测量使用隔离构建；工作区的 {len(meta["source_hashes_matching_tested_variant"])} 个生产源码/头文件及 Makefile 哈希与 tick 候选一致。工作区另行重新构建并检查；不把不同路径的动态库声称为字节完全相同。详细哈希见 [检查元数据](integration-metadata.json)。\n\n'
  detail+='| 检查命令 | 返回码 |\n| --- | ---: |\n'
  for c in meta['commands']:detail+='| `'+ ' '.join(c['command'])+f'` | {c["returncode"]} |\n'
  detail+='\n检查覆盖直接 C API、标准 LiTL C/C++、NDEBUG、无 shadow mutex、取消/超时及 24 线程/2 CPU 的超额订阅配置。完成时 sched_ext 为 disabled。构建与检查日志位于 logs.tar.gz 的 integration-check-*.log。\n'
  (OUT/'integration.md').write_text(detail)
 shutil.copy2(WORK/'cache-topology.json',OUT/'cache-topology.json')
 confirm=load('confirm');regression=load('regression')
 text='''# simplify：fullhook 差异归因与选择性迁入

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
'''
 for mode in ['readrandom','fillrandom']:
  for backend in ['mcs_accordin','mcs_tas_accordin']:
   base=group(confirm,mode,backend,'baseline');cur=group(confirm,mode,backend,'tick');bv=values(base,'ops_per_second');cv=values(cur,'ops_per_second');bm=mean(base,'ops_per_second');cm=mean(cur,'ops_per_second')
   ratio=f'{cm/bm:.3f}×' if len(bv)==len(cv)==3 else '—'
   text+=f'| {mode} | {backend} | {fmt(bm,1000)} | {fmt(cm,1000)} | {ratio} | {len(bv)}/3、{len(cv)}/3 |\n'
 text+='\n### 确认测量逐轮与波动\n\n| 工作负载 | 锁 | 版本 | 三次 Kops/s | CV |\n| --- | --- | --- | --- | ---: |\n'
 for mode in ['readrandom','fillrandom']:
  for backend in ['mcs_accordin','mcs_tas_accordin']:
   for name in ['baseline','tick']:
    g=group(confirm,mode,backend,name);v=values(g,'ops_per_second');samples=' / '.join(fmt(s.get('ops_per_second') if s['valid'] else None,1000) for s in g);cv=f'{100*statistics.stdev(v)/statistics.mean(v):.2f}%' if len(v)>1 else '—'
    text+=f'| {mode} | {backend} | {name} | {samples} | {cv} |\n'
 text+='\n## 完整 streamcluster 与 barrier 回归\n\nstreamcluster 参数为 `10 30 512 32768 32768 2000 none <output> 192`；时间使用原程序计数 / 100,000,000。每次输出必须匹配 SHA256 `dfeea2357203cceeb8bcdac4984ffc9da9c953f1f1d19c06990626a4575ef01f`。barrier 为 192 线程、100 轮。单位秒，越低越好。\n\n| 工作负载 | 锁 | simplify 基线 | 仅 tick | 基线/当前 | 有效次数 |\n| --- | --- | ---: | ---: | ---: | --- |\n'
 for mode in ['stream','barrier']:
  for backend in ['mcs_accordin','mcs_tas_accordin']:
   base=group(regression,mode,backend,'baseline');cur=group(regression,mode,backend,'tick');bv=values(base,'seconds');cv=values(cur,'seconds');bm=mean(base,'seconds');cm=mean(cur,'seconds');ratio=f'{bm/cm:.3f}×' if len(bv)==len(cv)==3 else '—'
   text+=f'| {mode} | {backend} | {fmt(bm)} | {fmt(cm)} | {ratio} | {len(bv)}/3、{len(cv)}/3 |\n'
 text+='''
## 归因证据

screen 中的 `compact` 只将外部 raw lock 的 tail/locked 改为 8 字节对齐，不把锁内嵌到应用对象；它不能独自检验与应用字段共享缓存行的影响。`tls` 仅改变 direct 库的 TLS 模型；`bpf_all` 迁入 fullhook 的 BPF 策略但去除 USER_CV 过滤，保持 simplify 的 word 编码。`signal` 是从设计方案导出的 signal-only 独立唤醒控制，保留 broadcast 接力，不是完整 fullhook condvar。

fullhook0/fullhook1000 是同一动态库、仅自旋预算不同。fullhook_tas 只将 fullhook 的队首 TTAS 改回反复 exchange；其余策略不变。fullhook_heap 将 raw lock 移到独立的 64 字节对齐分配，tail/locked 仍紧凑；fullhook_heap+padding 再将两字段分开对齐。这些存储控制同时包含必要的指针间接访问、冷路径分配和潜在的 NUMA first-touch 差异，不能将结果全部归为缓存行共享或声称仅改变了一条 cache 指令。
'''
 for stage in ['screen','combo-screen','reverse-ttas','read-interactions','storage-read']:
  rows=load(stage)
  text+=f'\n### {stage}\n\n| 工作负载 | 版本 | 有效/尝试 | 均值 Kops/s | 逐次 Kops/s |\n| --- | --- | ---: | ---: | --- |\n'
  for mode,name in dict.fromkeys((s['benchmark'],s.get('branch',s.get('variant'))) for s in rows):
   g=group(rows,mode,'mcs_tas_accordin',name);v=values(g,'ops_per_second');text+=f'| {mode} | {name} | {len(v)}/{len(g)} | {fmt(mean(g,"ops_per_second"),1000)} | '+ ' / '.join(fmt(s.get('ops_per_second') if s['valid'] else None,1000) for s in g)+' |\n'
 text+='''
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
sudo python3 target/leveldb-attribution-20260906/run.py --stage repeat-confirm \\
  --arms baseline tick --backends mcs_accordin mcs_tas_accordin --repeats 3
sudo python3 target/leveldb-attribution-20260906/regression.py --stage repeat-regression \\
  --arms baseline tick --repeats 3
```
'''
 complete=len(confirm)==24 and len(regression)==24 and all(s['valid'] for s in confirm+regression)
 text+=f'\n报告生成时间：{datetime.datetime.now(datetime.timezone.utc).isoformat()}。归档尝试 {len(allrows)} 次，有效 {sum(s["valid"] for s in allrows)} 次；确认与回归矩阵完整有效：{complete}。\n'
 (OUT/'README.md').write_text(text)
 print(json.dumps({'out':str(OUT),'attempts':len(allrows),'complete':complete}))
if __name__=='__main__':main()
