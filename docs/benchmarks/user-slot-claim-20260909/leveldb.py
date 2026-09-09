#!/usr/bin/env python3
"""Compare the two ACCORDIN_USER_CLAIM arms on LevelDB db_bench.

One build serves both arms: an arm is only an environment override, so each
pair of samples shares the binaries, the seed and the session. Run mechanics,
validity rules and sched_ext checks come from the cv-custody runner; only the
cell construction differs, because no worktree is checked out here.
"""
import argparse
import datetime
import fcntl
import importlib.util
import json
import os
import re
import shutil
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

_spec = importlib.util.spec_from_file_location(
    'cv_custody_run',
    Path(__file__).resolve().parents[1] / 'cv-custody-20260906/run.py')
b = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(b)

ROOT = b.ROOT
EXE = b.EXE
LOCK = b.LOCK
THREADS = b.THREADS
BACKENDS = b.BACKENDS

CLAIM_KEYS = ['renews', 'claims', 'undone', 'queued', 'adopted', 'swept',
              'slots_left', 'demand']
CLAIM_PATTERN = re.compile(r'\[accordin_claim\] ' + ' '.join(
    f'{k}=(-?\\d+)' for k in CLAIM_KEYS))

ARMS = {'claim': {}, 'yield': {'ACCORDIN_USER_CLAIM': '0'}}

# Throughput arms carry no counters: the counter atomics sit on the hot path.
BASE_CONFIG = {k: v for k, v in b.CONFIG.items() if k != 'ACCORDIN_CV_COUNTERS'}


def make_cell(arm, backend, counters):
    adapter = ROOT / f'third_party/litl/lib/lib{BACKENDS[backend]}_original.so'
    libdir = ROOT / 'target/release'
    direct = libdir / f'lib{backend}_direct.so'
    for path in (adapter, direct):
        if not path.is_file():
            raise RuntimeError(f'missing library {path}')
    env = dict(ARMS[arm])
    if counters:
        env['ACCORDIN_CV_COUNTERS'] = '1'
    return {'id': f'{arm}-{backend}' + ('-counters' if counters else ''),
            'arm': arm + ('-counters' if counters else ''), 'backend': backend,
            'commit': b.command_output(['git', '-C', str(ROOT), 'rev-parse', 'HEAD']),
            'library': adapter, 'libdir': libdir, 'env': env,
            'expected_maps': sorted(str(p.resolve()) for p in (adapter, direct))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--seed', type=Path, default=b.DEFAULT_SEED)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--benchmarks', default='readrandom,fillrandom')
    parser.add_argument('--backends', default=','.join(BACKENDS))
    parser.add_argument('--max-attempts', type=int, default=2)
    parser.add_argument('--counters', action='store_true',
                        help='collect the [accordin_claim] line instead of throughput')
    args = parser.parse_args()

    benchmarks = [m for m in ('readrandom', 'fillrandom') if m in args.benchmarks.split(',')]
    backends = [x for x in BACKENDS if x in args.backends.split(',')]
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)

    b.CONFIG = dict(BASE_CONFIG)
    b.ENV = {k: v for k, v in os.environ.items() if not k.startswith(b.SCRUB)} | b.CONFIG
    b.COUNTER_KEYS = CLAIM_KEYS
    b.COUNTER_PATTERN = CLAIM_PATTERN

    guard = os.open(LOCK, os.O_RDONLY)
    fcntl.flock(guard, fcntl.LOCK_EX)
    cpus = b.online_cpus()
    b.wait_disabled()
    os.sched_setaffinity(0, cpus)
    if b.sha(EXE) != b.EXPECTED_EXE:
        raise RuntimeError(f'db_bench sha256 mismatch: {b.sha(EXE)}')
    if not args.seed.is_dir():
        raise RuntimeError(f'missing seed at {args.seed}')

    # Arms alternate inside a backend so a pair of samples is adjacent in time.
    cells = [make_cell(arm, backend, args.counters)
             for backend in backends for arm in ARMS]
    files = {EXE, Path(__file__).resolve()}
    for cell in cells:
        files.update(Path(p) for p in cell['expected_maps'])
    hashes = {str(p): b.sha(p) for p in sorted(files)}
    seed_manifest = b.inventory(args.seed)

    meta = {'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
            'commit': cells[0]['commit'], 'arms': ARMS, 'counters': args.counters,
            'uname': list(os.uname()), 'hashes': hashes, 'seed': str(args.seed),
            'seed_manifest_files': len(seed_manifest), 'configuration': b.CONFIG,
            'threads': THREADS, 'cpus': cpus,
            'cpu_online_spec': b.read('/sys/devices/system/cpu/online'),
            'num_keys': 1000000, 'value_bytes': 100, 'requested_time_ms': b.TIME_MS,
            'db_filesystem': '/tmp tmpfs', 'repeats': args.repeats,
            'benchmarks': benchmarks, 'backends': backends,
            'max_attempts': args.max_attempts,
            'cpu_governors': sorted(set(b.read(p) for p in Path(
                '/sys/devices/system/cpu').glob('cpu[0-9]*/cpufreq/scaling_governor'))),
            'dmesg_available': b.dmesg_lines() is not None}
    (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')

    samples = b.load_results(out / 'results.jsonl')
    if samples:
        print(f'RESUME {len(samples)} samples', flush=True)
    tmp = Path(tempfile.mkdtemp(prefix='accordin-user-claim-', dir='/tmp'))
    exhausted = []
    try:
        if 'readrandom' in benchmarks and not (out / 'seed-audit.log').exists():
            audit = tmp / 'seed-audit'
            shutil.copytree(args.seed, audit)
            with (out / 'seed-audit.log').open('w') as f:
                result = subprocess.run(
                    [str(EXE), '--benchmarks=readseq', '--threads=1',
                     '--use_existing_db=1', f'--db={audit}'],
                    env=b.ENV, stdout=f, stderr=subprocess.STDOUT, timeout=180)
            if result.returncode != 0 or not re.search(
                    r'BENCH_TOTAL ops=1000000 ', (out / 'seed-audit.log').read_text()):
                raise RuntimeError('seed audit failed')
            shutil.rmtree(audit)
            print('Seed audit: 1000000 entries', flush=True)

        for rep in range(1, args.repeats + 1):
            rotate = (rep - 1) % len(cells)
            order = cells[rotate:] + cells[:rotate]
            for mode in benchmarks:
                for cell in order:
                    key = (mode, cell['backend'], cell['arm'], rep)
                    done = [s for s in samples if b.cell_key(s) == key]
                    if any(s.get('valid') for s in done):
                        print(f'SKIP {mode}-{cell["id"]}-r{rep}', flush=True)
                        continue
                    for attempt in range(len(done) + 1, args.max_attempts + 1):
                        db = tmp / f'{mode}-{cell["id"]}-r{rep}-a{attempt}'
                        if mode == 'readrandom':
                            shutil.copytree(args.seed, db)
                            if b.inventory(db) != seed_manifest:
                                raise RuntimeError('seed copy differs from seed')
                        sample = b.run(out, db, mode, cell, rep, attempt, cpus, b.TIME_MS)
                        samples.append(sample)
                        shutil.rmtree(db, ignore_errors=True)
                        if sample['valid']:
                            break
                        print(f'INVALID {sample["id"]}: {sample["reason"]}', flush=True)
                    else:
                        exhausted.append({'benchmark': mode, 'backend': cell['backend'],
                                          'arm': cell['arm'], 'repeat': rep})
                        print(f'EXHAUSTED {mode}-{cell["id"]}-r{rep}', flush=True)

        if b.inventory(args.seed) != seed_manifest:
            raise RuntimeError('seed changed during the session')
        changed = [p for p, h in hashes.items() if b.sha(p) != h]
        if changed:
            raise RuntimeError(f'inputs changed during the session: {changed}')

        arm_names = sorted({c['arm'] for c in cells}, key=lambda n: 0 if 'yield' in n else 1)
        arms = [{'name': n, 'resolved_commit': cells[0]['commit'],
                 'env': next(c['env'] for c in cells if c['arm'] == n)} for n in arm_names]
        baseline_arm, rows = b.summarize(samples, arms, benchmarks, backends)
        invalid = [{'id': s['id'], 'benchmark': s['benchmark'], 'backend': s['backend'],
                    'arm': s['arm'], 'repeat': s['repeat'], 'attempt': s.get('attempt'),
                    'reason': s.get('reason', 'unknown')}
                   for s in samples if not s.get('valid')]
        (out / 'summary.json').write_text(json.dumps(
            {'baseline_arm': baseline_arm, 'cells': rows, 'invalid_runs': invalid,
             'cells_without_valid_sample': exhausted}, indent=2) + '\n')
        b.write_markdown(out / 'summary.md', baseline_arm, rows, samples)
        meta.update(completed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                    valid_runs=sum(1 for s in samples if s.get('valid')),
                    invalid_runs=len(invalid), cells_without_valid_sample=exhausted,
                    hashes_unchanged=True, seed_unchanged=True, state_after=b.state())
        (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')
        shutil.rmtree(tmp, ignore_errors=True)
        b.fix_owner([out])
        print('SUMMARY ' + json.dumps(rows), flush=True)
        return 1 if exhausted else 0
    except BaseException:
        b.fix_owner([out])
        print(f'Databases retained in {tmp}', flush=True)
        raise


if __name__ == '__main__':
    raise SystemExit(main())
