#!/usr/bin/env python3
"""Run streamcluster once per ACCORDIN_USER_CLAIM arm as a fairness regression check.

Both arms share this worktree's build; only the environment variable differs.
Run mechanics and validity rules come from the cv-custody streamcluster runner,
and the reported time is the runner's own wall clock, not the counter the
program derives from tsc_khz.
"""
import argparse
import datetime
import fcntl
import importlib.util
import json
import os
import statistics
from pathlib import Path

_spec = importlib.util.spec_from_file_location(
    'cv_custody_streamcluster',
    Path(__file__).resolve().parents[1] / 'cv-custody-20260906/streamcluster.py')
sc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(sc)
b = sc.b

ROOT = b.ROOT
LEVELDB = importlib.util.spec_from_file_location(
    'user_claim_leveldb', Path(__file__).resolve().with_name('leveldb.py'))
_ldb = importlib.util.module_from_spec(LEVELDB)
LEVELDB.loader.exec_module(_ldb)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--repeats', type=int, default=1)
    parser.add_argument('--modes', nargs='+', default=['stream'], choices=sc.MODES)
    parser.add_argument('--backends', default=','.join(b.BACKENDS))
    parser.add_argument('--max-attempts', type=int, default=2)
    parser.add_argument('--counters', action='store_true')
    args = parser.parse_args()

    modes = [m for m in sc.MODES if m in args.modes]
    backends = [x for x in b.BACKENDS if x in args.backends.split(',')]
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)

    b.CONFIG = dict(_ldb.BASE_CONFIG)
    b.ENV = {k: v for k, v in os.environ.items() if not k.startswith(b.SCRUB)} | b.CONFIG
    b.COUNTER_KEYS = _ldb.CLAIM_KEYS
    b.COUNTER_PATTERN = _ldb.CLAIM_PATTERN

    guard = os.open(b.LOCK, os.O_RDONLY)
    fcntl.flock(guard, fcntl.LOCK_EX)
    cpus = b.online_cpus()
    b.wait_disabled()
    os.sched_setaffinity(0, cpus)
    binaries = {'stream': sc.locate(sc.STREAM), 'barrier': sc.locate(sc.BARRIER)}
    accepted = set(sc.ACCEPTED_STREAM_SHA)

    cells = [_ldb.make_cell(arm, backend, args.counters)
             for arm in _ldb.ARMS for backend in backends]
    files = set(binaries.values()) | {Path(__file__).resolve()}
    for cell in cells:
        files.update(Path(p) for p in cell['expected_maps'])
    hashes = {str(p): b.sha(p) for p in sorted(files)}

    meta = {'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
            'commit': cells[0]['commit'], 'arms': _ldb.ARMS, 'counters': args.counters,
            'uname': list(os.uname()), 'hashes': hashes,
            'benchmark_binaries': {k: {'path': str(v), 'sha256': b.sha(v)}
                                   for k, v in binaries.items()},
            'accepted_stream_output_sha256': sorted(accepted),
            'configuration': b.CONFIG, 'threads': sc.THREADS, 'cpus': cpus,
            'cpu_online_spec': b.read('/sys/devices/system/cpu/online'),
            'repeats': args.repeats, 'modes': modes, 'backends': backends,
            'max_attempts': args.max_attempts,
            'cpu_governors': sorted(set(b.read(p) for p in Path(
                '/sys/devices/system/cpu').glob('cpu[0-9]*/cpufreq/scaling_governor'))),
            'dmesg_available': b.dmesg_lines() is not None}
    (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')

    samples = b.load_results(out / 'results.jsonl')
    exhausted = []
    for rep in range(1, args.repeats + 1):
        rotate = (rep - 1) % len(cells)
        order = cells[rotate:] + cells[:rotate]
        for mode in modes:
            for cell in order:
                key = (mode, cell['backend'], cell['arm'], rep)
                done = [s for s in samples if b.cell_key(s) == key]
                if any(s.get('valid') for s in done):
                    print(f'SKIP {mode}-{cell["id"]}-r{rep}', flush=True)
                    continue
                for attempt in range(len(done) + 1, args.max_attempts + 1):
                    sample = sc.run(out, mode, cell, rep, attempt, cpus, binaries, accepted)
                    samples.append(sample)
                    if sample['valid']:
                        break
                    print(f'INVALID {sample["id"]}: {sample["reason"]}', flush=True)
                else:
                    exhausted.append({'benchmark': mode, 'backend': cell['backend'],
                                      'arm': cell['arm'], 'repeat': rep})

    rows = []
    for mode in modes:
        for backend in backends:
            def wall(name):
                return [s['wall_seconds'] for s in samples if s.get('valid')
                        and s['benchmark'] == mode and s['backend'] == backend
                        and s['arm'] == name]
            base = wall('yield')
            for arm in sorted({c['arm'] for c in cells}):
                vals = wall(arm)
                counters = [s['counters'] for s in samples if s.get('valid')
                            and s['benchmark'] == mode and s['backend'] == backend
                            and s['arm'] == arm and 'counters' in s]
                rows.append({
                    'benchmark': mode, 'backend': backend, 'arm': arm,
                    'n_valid': len(vals),
                    'n_invalid': sum(1 for s in samples if not s.get('valid')
                                     and s['benchmark'] == mode and s['backend'] == backend
                                     and s['arm'] == arm),
                    'wall_seconds': vals,
                    'mean_wall_seconds': statistics.mean(vals) if vals else None,
                    'counter_seconds': [s['seconds'] for s in samples if s.get('valid')
                                        and s['benchmark'] == mode and s['backend'] == backend
                                        and s['arm'] == arm],
                    'relative_to_yield': (statistics.mean(vals) / statistics.mean(base)
                                          if vals and base else None),
                    'claim_counters': counters})
    (out / 'summary.json').write_text(json.dumps(
        {'cells': rows, 'cells_without_valid_sample': exhausted,
         'invalid_runs': [{'id': s['id'], 'reason': s.get('reason')}
                          for s in samples if not s.get('valid')]}, indent=2) + '\n')
    changed = [p for p, h in hashes.items() if b.sha(p) != h]
    meta.update(completed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                inputs_changed=changed, state_after=b.state())
    (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')
    b.fix_owner([out])
    print('SUMMARY ' + json.dumps(rows), flush=True)
    return 1 if exhausted else 0


if __name__ == '__main__':
    raise SystemExit(main())
