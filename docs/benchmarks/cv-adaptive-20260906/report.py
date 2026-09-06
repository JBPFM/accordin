#!/usr/bin/env python3
"""Summarize the complete confirmation matrix without mixing pilot samples."""
import csv
import datetime
import json
from pathlib import Path
import statistics

WORK = Path(__file__).resolve().parent


def rows(stage):
    return [json.loads(line) for line in (WORK / stage / 'results.jsonl').read_text().splitlines()]


def group(data, mode, backend, name):
    return [r for r in data if r['benchmark'] == mode and r['backend'] == backend
            and r.get('branch', r.get('variant')) == name]


def metric(data, mode, backend, name):
    selected = group(data, mode, backend, name)
    assert len(selected) == 3 and all(r['valid'] for r in selected)
    key = 'seconds' if mode == 'stream' else 'ops_per_second'
    return [r[key] for r in selected]


def main():
    original_ldb = rows('confirm-leveldb')
    original_stream = rows('confirm-stream')
    ldb = original_ldb + rows('final-leveldb') + rows('default-mcs-leveldb')
    stream = original_stream + rows('final-stream') + rows('default-mcs-stream')
    assert len(original_ldb) == 24 and len(original_stream) == 12
    assert len(ldb) == 42 and len(stream) == 21
    assert len(rows('closing-baseline-leveldb')) == 4
    assert len(rows('closing-baseline-stream')) == 2
    assert len(rows('default-tas-leveldb')) == 2 and len(rows('default-tas-stream')) == 1
    closing = rows('closing-baseline-leveldb') + rows('closing-baseline-stream')
    text = (WORK / 'methods.md').read_text()
    text += '\n## 所选预算的三轮结果\n\n'
    text += '| 工作负载 | 后端 / 预算 | 原基线三轮均值 | 末尾基线单次 | 当前三轮均值 | 相对原基线 |\n| --- | --- | ---: | ---: | ---: | ---: |\n'
    summaries = []
    for mode in ['readrandom', 'fillrandom', 'stream']:
        for backend in ['mcs_accordin', 'mcs_tas_accordin']:
            data = stream if mode == 'stream' else ldb
            before = metric(data, mode, backend, 'baseline')
            candidate = 'defaults' if backend == 'mcs_accordin' else 'pressure'
            budget = 0 if backend == 'mcs_accordin' else 50
            after = metric(data, mode, backend, candidate)
            a, b = statistics.mean(before), statistics.mean(after)
            scale = 1 if mode == 'stream' else 1000
            unit = '秒' if mode == 'stream' else 'Kops/s'
            change = (b / a - 1) * 100
            tail = group(closing, mode, backend, 'baseline')
            assert len(tail) == 1 and tail[0]['valid']
            tail_value = tail[0]['seconds' if mode == 'stream' else 'ops_per_second']
            text += f'| {mode} | {backend} / {budget} µs | {a/scale:.3f} {unit} | {tail_value/scale:.3f} {unit} | {b/scale:.3f} {unit} | {change:+.2f}% |\n'
            summaries.append(dict(benchmark=mode, backend=backend, baseline=before,
                                  current=after, baseline_mean=a, current_mean=b,
                                  change_percent=change, closing_baseline=tail_value,
                                  source_variant=candidate, default_spin_us=budget))
    text += '\n吞吐量越高越好；streamcluster 耗时越低越好。变化为当前/原基线 − 1，不把探索样本混入确认均值。MCS 写入基线在末尾明显上移，不能把相对早期基线的百分比当作稳定收益；因此 MCS 默认关闭自旋。TAS 三轮数据来自相同 50 µs 路径的 pressure 构建，调整 MCS 默认值后的 defaults 构建另有一次完整应用复查。\n'
    text += '\n### 逐轮数据\n\n| 工作负载 | 后端 | 版本 | 三轮数值 | 标准差/均值 |\n| --- | --- | --- | --- | ---: |\n'
    for s in summaries:
        scale = 1 if s['benchmark'] == 'stream' else 1000
        for name in ['baseline', 'current']:
            vals = s[name]
            rendered = ' / '.join(f'{v/scale:.3f}' for v in vals)
            cv = 100 * statistics.stdev(vals) / statistics.mean(vals)
            text += f'| {s["benchmark"]} | {s["backend"]} | {name} | {rendered} | {cv:.2f}% |\n'
    text += '\n## 调整默认值后的 TAS 构建复查\n\n以下为 defaults 新构建的单次结果，不并入 pressure 的三轮均值。两者 TAS 路径和预算相同。\n\n'
    text += '| 工作负载 | 单次结果 |\n| --- | ---: |\n'
    for r in rows('default-tas-leveldb') + rows('default-tas-stream'):
        assert r['valid']
        value = r['seconds'] if r['benchmark'] == 'stream' else r['ops_per_second'] / 1000
        unit = '秒' if r['benchmark'] == 'stream' else 'Kops/s'
        text += f'| {r["benchmark"]} | {value:.3f} {unit} |\n'
    text += '\n## 未默认启用的 MCS 50 µs 候选\n\n保留 pressure 的完整三轮，说明为何将 MCS 的默认预算改回 0。写入结果与末尾基线相当，streamcluster 耗时则更高。\n\n'
    text += '| 工作负载 | 三轮数值（Kops/s 或秒） | 均值 |\n| --- | --- | ---: |\n'
    for mode in ['readrandom', 'fillrandom', 'stream']:
        data = stream if mode == 'stream' else ldb
        scale = 1 if mode == 'stream' else 1000
        vals = metric(data, mode, 'mcs_accordin', 'pressure')
        rendered = ' / '.join(f'{v/scale:.3f}' for v in vals)
        text += f'| {mode} | {rendered} | {statistics.mean(vals)/scale:.3f} |\n'
    text += '\n## 需求提示优化之前的配对确认\n\n`balanced` 发布队列长度，`pressure` 使用最终的队列非空提示，两个后端均为 50 µs。下表仅使用第一阶段基线与 balanced 交错测量的三轮数据。\n\n'
    text += '| 工作负载 | 后端 | 基线 | balanced | 变化 |\n| --- | --- | ---: | ---: | ---: |\n'
    for mode in ['readrandom', 'fillrandom', 'stream']:
        data = original_stream if mode == 'stream' else original_ldb
        scale = 1 if mode == 'stream' else 1000
        for backend in ['mcs_accordin', 'mcs_tas_accordin']:
            a = statistics.mean(metric(data, mode, backend, 'baseline'))
            b = statistics.mean(metric(data, mode, backend, 'balanced'))
            text += f'| {mode} | {backend} | {a/scale:.3f} | {b/scale:.3f} | {(b/a-1)*100:+.2f}% |\n'
    text += '\n## 探索与补充对照\n\n下表均为单次探索或末尾基线复查，不混入三轮确认均值。`adaptive`/`fixed` 是初版等待接力 wake 的实现；`notified`/`balanced` 以逻辑通知结束 CV 自旋。`fixed` 和 `balanced-fixed` 只禁用按历史评分响应队列压力的判断，仍保留预算和抢占撤销。\n\n'
    text += '| 阶段 | 工作负载 | 后端 | 版本 | 自旋上限 µs | 数值（Kops/s 或秒） | 有效 |\n| --- | --- | --- | --- | ---: | ---: | --- |\n'
    all_rows = []
    for stage in sorted(WORK.iterdir()):
        if not stage.is_dir() or not (stage / 'results.jsonl').exists():
            continue
        data = rows(stage.name)
        all_rows.extend(dict(stage=stage.name, **r) for r in data)
        if stage.name.startswith(('confirm-', 'final-', 'default-mcs-')):
            continue
        for r in data:
            value = r.get('seconds') if r['benchmark'] == 'stream' else r.get('ops_per_second', 0) / 1000
            text += f'| {stage.name} | {r["benchmark"]} | {r["backend"]} | {r.get("branch",r.get("variant"))} | {r["environment"].get("ACCORDIN_CV_SPIN_US", "默认")} | {value:.3f} | {r["valid"]} |\n'
    (WORK / 'summary.json').write_text(json.dumps(summaries, indent=2) + '\n')
    with (WORK / 'results.csv').open('w', newline='') as f:
        fields = ['stage', 'id', 'benchmark', 'variant', 'backend', 'repeat', 'spin_us',
                  'ops_per_second', 'seconds', 'valid', 'returncode', 'timeout']
        writer = csv.DictWriter(f, fieldnames=fields, lineterminator='\n')
        writer.writeheader()
        for r in all_rows:
            row = {k: r.get(k) for k in fields}
            row.update(variant=r.get('branch', r.get('variant')),
                       spin_us=r['environment'].get('ACCORDIN_CV_SPIN_US'))
            writer.writerow(row)
    text += f'\n记录生成时间：{datetime.datetime.now(datetime.timezone.utc).isoformat()}。已归档 {len(all_rows)} 次应用运行，其中 {sum(r["valid"] for r in all_rows)} 次有效。\n'
    (WORK / 'README.md').write_text(text)


if __name__ == '__main__':
    main()
