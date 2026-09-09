#!/usr/bin/env python3
"""Render the tables used by README.md from the raw result directories."""
import argparse
import json
import statistics
from pathlib import Path


def load(path):
    return json.loads(Path(path).read_text())


def jsonl(path):
    p = Path(path)
    return [json.loads(l) for l in p.read_text().splitlines() if l.strip()] if p.is_file() else []


def fmt(value, digits=3):
    return '—' if value is None else f'{value:.{digits}f}'


def mutexbench(root):
    rows = load(root / 'summary.json')
    out = ['### sched_yield 次数与吞吐量', '',
           '| 配置 | arm | 三次 sched_yield 次数 | 均值 | claim/yield | 三次 Kops/s | 均值 Kops/s | claim/yield |',
           '|---|---|---|---:|---:|---|---:|---:|']
    for r in rows:
        y = ' / '.join(f'{v:,}' for v in r['sched_yield_entries'])
        t = ' / '.join(f'{v / 1000:.1f}' for v in r['throughput_ops_per_sec'])
        out.append(
            f'| {r["case"]} | {r["arm"]} | {y} | {r["mean_sched_yield_entries"]:,.0f} | '
            + (fmt(r['yields_relative_to_yield_arm']) if r['arm'] == 'claim' else '—')
            + f' | {t} | {r["mean_throughput_ops_per_sec"] / 1000:.1f} | '
            + (fmt(r['throughput_relative_to_yield_arm']) if r['arm'] == 'claim' else '—') + ' |')
    keys = ['renews', 'claims', 'undone', 'queued', 'adopted', 'swept', 'slots_left', 'demand']
    out += ['', '### 计数器（单独一次开启 ACCORDIN_CV_COUNTERS 的运行）', '',
            '| 配置 | arm | ' + ' | '.join(keys) + ' |',
            '|---|---|' + '---:|' * len(keys)]
    for r in rows:
        c = r['claim_counters']
        out.append(f'| {r["case"]} | {r["arm"]} | ' + ' | '.join(f'{c[k]:,}' for k in keys) + ' |')
    return '\n'.join(out)


def leveldb(root, counters_root):
    data = load(root / 'summary.json')
    out = ['| 工作负载 | 后端 | arm | 三次 Kops/s | 均值 Kops/s | CV | claim/yield |',
           '|---|---|---|---|---:|---:|---:|']
    for cell in data['cells']:
        runs = ' / '.join(f'{v / 1000:.3f}' for v in cell['runs_ops_per_second']) or '—'
        mean = cell['mean_ops_per_second']
        rel = cell.get('relative_to_yield')
        out.append(
            f'| {cell["benchmark"]} | {cell["backend"]} | {cell["arm"]} | {runs} | '
            + (f'{mean / 1000:.3f}' if mean else '—') + ' | '
            + (f'{cell["cv_percent"]:.2f}%' if cell.get('cv_percent') else '—') + ' | '
            + (fmt(rel) if cell['arm'] == 'claim' else '—') + ' |')
    lines = ['\n'.join(out)]
    if counters_root and (Path(counters_root) / 'results.jsonl').is_file():
        keys = ['renews', 'claims', 'undone', 'queued', 'adopted', 'swept', 'slots_left', 'demand']
        lines += ['', '### 计数器运行（单独一轮，ACCORDIN_CV_COUNTERS=1）', '',
                  '| 工作负载 | 后端 | arm | ' + ' | '.join(keys) + ' | Kops/s |',
                  '|---|---|---|' + '---:|' * (len(keys) + 1)]
        for s in jsonl(Path(counters_root) / 'results.jsonl'):
            if not s.get('valid') or 'counters' not in s:
                continue
            c = s['counters']
            lines.append(f'| {s["benchmark"]} | {s["backend"]} | {s["arm"]} | '
                         + ' | '.join(f'{c[k]:,}' for k in keys)
                         + f' | {s["ops_per_second"] / 1000:.3f} |')
    return '\n'.join(lines)


def latency(root):
    rows = load(Path(root) / 'summary.json')
    out = ['| 范围 | arm | 样本数 | p50 (us) | p99 (us) | max (us) | 均值 (us) | >2ms | '
           'NORMAL_DSQ 插入次数 | 该轮 Kops/s |',
           '|---|---|---:|---:|---:|---:|---:|---:|---:|---:|']
    for scope, key, inserts in (('db_bench 进程', None, 'inserts'),
                                ('全机', 'all_tasks', 'inserts_all')):
        for r in rows:
            h = r['histogram'] or {}
            h = h.get(key, {}) if key else h
            out.append(f'| {scope} | {r["arm"]} | {h.get("samples", 0):,} | {h.get("p50")} | '
                       f'{h.get("p99")} | {h.get("max_us")} | {h.get("average_us")} | '
                       f'{h.get("over_2ms", 0):,} | '
                       f'{(r["histogram"] or {}).get(inserts, 0):,} | '
                       f'{r["ops_per_second"] / 1000:.3f} |')
    return '\n'.join(out)


def streamcluster(root):
    data = load(Path(root) / 'summary.json')
    out = ['| 后端 | arm | 墙钟秒 | 均值 | claim/yield | 程序内计时秒 |',
           '|---|---|---|---:|---:|---|']
    for cell in data['cells']:
        runs = ' / '.join(f'{v:.3f}' for v in cell['wall_seconds']) or '—'
        counter = ' / '.join(f'{v:.3f}' for v in cell['counter_seconds']) or '—'
        out.append(f'| {cell["backend"]} | {cell["arm"]} | {runs} | '
                   + fmt(cell['mean_wall_seconds']) + ' | '
                   + (fmt(cell['relative_to_yield']) if cell['arm'] == 'claim' else '—')
                   + f' | {counter} |')
    return '\n'.join(out)


CSV_COLUMNS = ['id', 'benchmark', 'case', 'backend', 'arm', 'repeat', 'attempt', 'valid',
               'reason', 'ops_per_second', 'throughput_ops_per_sec', 'sched_yield_entries',
               'operations', 'roi_seconds', 'seconds', 'wall_seconds', 'max_process_threads',
               'output_sha256']


def write_csv(directory):
    """Flatten one results.jsonl into the columns the tables are built from."""
    import csv
    rows = jsonl(Path(directory) / 'results.jsonl')
    if not rows:
        return
    counter_keys = sorted({k for r in rows for k in (r.get('counters')
                                                     or r.get('claim_counters') or {})})
    with (Path(directory) / 'results.csv').open('w', newline='') as f:
        writer = csv.writer(f)
        writer.writerow(CSV_COLUMNS + counter_keys)
        for r in rows:
            counters = r.get('counters') or r.get('claim_counters') or {}
            writer.writerow([r.get(c, '') for c in CSV_COLUMNS]
                            + [counters.get(k, '') for k in counter_keys])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--write-csv', action='store_true')
    args = parser.parse_args()
    root = args.root.resolve()
    if args.write_csv:
        for directory in sorted(p for p in root.iterdir() if p.is_dir()):
            write_csv(directory)
    sections = [('mutexbench', lambda: mutexbench(root / 'mutexbench')),
                ('leveldb', lambda: leveldb(root / 'leveldb', root / 'leveldb-counters')),
                ('latency', lambda: latency(root / 'normal-dsq-latency')),
                ('streamcluster', lambda: streamcluster(root / 'streamcluster'))]
    for name, render in sections:
        print(f'\n===== {name} =====\n')
        try:
            print(render())
        except FileNotFoundError as exc:
            print(f'(missing: {exc})')


if __name__ == '__main__':
    main()
