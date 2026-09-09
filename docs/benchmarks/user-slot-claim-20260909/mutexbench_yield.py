#!/usr/bin/env python3
"""Count sched_yield entries and throughput for the two ACCORDIN_USER_CLAIM arms.

One build, one session: the arms differ only by the environment variable, so
every pair of samples comes from the same binaries. The LiTL
mcsaccordin_original adapter interposes the pthread mutex used by mutex_bench.
"""
import argparse
import datetime
import fcntl
import importlib.util
import json
import os
import re
import statistics
import subprocess
import time
from pathlib import Path

_spec = importlib.util.spec_from_file_location(
    'cv_custody_run',
    Path(__file__).resolve().parents[1] / 'cv-custody-20260906/run.py')
b = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(b)

ROOT = b.ROOT
BINARY = ROOT / 'bench/mutexbench/mutex_bench'
ADAPTER = ROOT / 'third_party/litl/lib/libmcsaccordin_original.so'
LIBDIR = ROOT / 'target/release'
DIRECT = LIBDIR / 'libmcs_accordin_direct.so'
LOCK = b.LOCK
TRACEPOINT = 'syscalls:sys_enter_sched_yield'

CONFIG = {
    'LC_ALL': 'C',
    'MCS_ACCORDIN_DIRECT_DISABLE_BPF': '0',
    'MCS_ACCORDIN_DIRECT_STATS_ONLY': '0',
    'ACCORDIN_DISABLE_ADMISSION': '0',
    'ACCORDIN_HOOK_STATS': '0',
}
# The interposer belongs to the benchmark alone: preloading it into perf would
# make perf load the scheduler as well.
PRELOAD = {'LD_PRELOAD': str(ADAPTER), 'LD_LIBRARY_PATH': str(LIBDIR)}
ENV = {k: v for k, v in os.environ.items() if not k.startswith(b.SCRUB)} | CONFIG

ARMS = {'claim': {}, 'yield': {'ACCORDIN_USER_CLAIM': '0'}}

# threads, critical ns, outside ns
CASES = {
    'low-t2-c100-o0': (2, 100, 0),
    'low-t4-c100-o0': (4, 100, 0),
    'low-t4-c100-o3000': (4, 100, 3000),
    'overload-t96-c100-o3000': (96, 100, 3000),
}

CLAIM_KEYS = ['renews', 'claims', 'undone', 'queued', 'adopted', 'swept',
              'slots_left', 'demand']
CLAIM_PATTERN = re.compile(r'\[accordin_claim\] ' + ' '.join(
    f'{k}=(-?\\d+)' for k in CLAIM_KEYS))
METRIC_PATTERN = {
    'throughput_ops_per_sec': re.compile(r'^throughput_ops_per_sec: ([\d.]+)$', re.M),
    'total_operations': re.compile(r'^total_operations: (\d+)$', re.M),
    'elapsed_seconds': re.compile(r'^elapsed_seconds: ([\d.]+)$', re.M),
    'avg_lock_hold_ns': re.compile(r'^avg_lock_hold_ns: ([\d.]+)$', re.M),
}


def one(out, case, arm, rep, duration_ms, warmup_ms, counters):
    b.wait_disabled()
    threads, critical, outside = CASES[case]
    ident = f'{case}-{arm}-r{rep}' + ('-counters' if counters else '')
    perf_path = out / (ident + '.perf')
    log_path = out / (ident + '.log')
    bench = [str(BINARY), '--threads', str(threads), '--duration-ms', str(duration_ms),
             '--warmup-duration-ms', str(warmup_ms), '--critical-ns', str(critical),
             '--outside-ns', str(outside), '--lock-kind', 'mutex']
    overrides = dict(ARMS[arm])
    if counters:
        overrides['ACCORDIN_CV_COUNTERS'] = '1'
    inner = PRELOAD | overrides
    cmd = (['perf', 'stat', '-e', TRACEPOINT, '-x', ',', '-o', str(perf_path), '--', 'env']
           + [f'{k}={v}' for k, v in inner.items()] + bench)
    sample = {'id': ident, 'case': case, 'arm': arm, 'repeat': rep, 'counters_enabled': counters,
              'threads': threads, 'critical_ns': critical, 'outside_ns': outside,
              'duration_ms': duration_ms, 'warmup_ms': warmup_ms,
              'command': bench, 'environment': CONFIG | inner,
              'seq_before': b.seq(), 'scx_enabled': False}
    print('START ' + ident, flush=True)
    start = time.monotonic()
    with log_path.open('w') as log:
        process = subprocess.Popen(cmd, env=ENV, stdout=log,
                                   stderr=subprocess.STDOUT, start_new_session=True,
                                   cwd=str(BINARY.parent))
        try:
            while process.poll() is None and time.monotonic() - start < duration_ms / 1000 + 90:
                sample['scx_enabled'] |= b.state() == 'enabled'
                time.sleep(0.005)
            sample['timeout'] = process.poll() is None
        finally:
            b.stop(process)
    sample.update(returncode=process.returncode, wall_seconds=time.monotonic() - start,
                  seq_after=b.seq(), state_after=b.state())
    text = log_path.read_text()
    for key, pattern in METRIC_PATTERN.items():
        match = pattern.search(text)
        if match:
            sample[key] = float(match[1])
    claim = CLAIM_PATTERN.findall(text)
    if claim:
        sample['claim_counters'] = {k: int(v) for k, v in zip(CLAIM_KEYS, claim[-1])}
    yields = None
    perf_text = perf_path.read_text() if perf_path.is_file() else ''
    for line in perf_text.splitlines():
        fields = line.split(',')
        if len(fields) > 2 and fields[2] == TRACEPOINT:
            yields = None if fields[0].startswith('<') else int(fields[0])
    sample['sched_yield_entries'] = yields
    reasons = []
    if sample['timeout']:
        reasons.append('timeout')
    if process.returncode != 0:
        reasons.append(f'returncode {process.returncode}')
    if 'throughput_ops_per_sec' not in sample:
        reasons.append('no throughput line')
    if yields is None:
        reasons.append('tracepoint not counted')
    if not sample['scx_enabled']:
        reasons.append('sched_ext never enabled')
    if sample['seq_after'] != sample['seq_before'] + 1:
        reasons.append(f'seq {sample["seq_before"]}->{sample["seq_after"]}')
    if re.search(r'Failed to (?:load|attach|register)|DEBUG DUMP|Assertion.*failed', text):
        reasons.append('log error')
    sample['valid'] = not reasons
    if reasons:
        sample['reason'] = '; '.join(reasons)
    with (out / 'results.jsonl').open('a') as f:
        f.write(json.dumps(sample) + '\n')
    print('DONE ' + json.dumps({k: sample.get(k) for k in
                                ['id', 'valid', 'reason', 'sched_yield_entries',
                                 'throughput_ops_per_sec', 'claim_counters']
                                if k in sample}), flush=True)
    return sample


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--duration-ms', type=int, default=5000)
    parser.add_argument('--warmup-ms', type=int, default=1000)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    samples = b.load_results(out / 'results.jsonl')
    done = {s['id'] for s in samples if s.get('valid')}

    guard = os.open(LOCK, os.O_RDONLY)
    fcntl.flock(guard, fcntl.LOCK_EX)
    cpus = b.online_cpus()
    b.wait_disabled()
    os.sched_setaffinity(0, cpus)
    files = [BINARY, ADAPTER, DIRECT, Path(__file__).resolve()]
    hashes = {str(p): b.sha(p) for p in files}
    meta = {'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
            'commit': b.command_output(['git', '-C', str(ROOT), 'rev-parse', 'HEAD']),
            'uname': list(os.uname()), 'cpus': cpus, 'hashes': hashes,
            'configuration': CONFIG, 'preload': PRELOAD, 'arms': ARMS, 'cases': CASES,
            'tracepoint': TRACEPOINT, 'repeats': args.repeats,
            'duration_ms': args.duration_ms, 'warmup_ms': args.warmup_ms,
            'cpu_governors': sorted(set(b.read(p) for p in Path(
                '/sys/devices/system/cpu').glob('cpu[0-9]*/cpufreq/scaling_governor')))}
    (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')

    for case in CASES:
        for rep in range(1, args.repeats + 1):
            order = list(ARMS) if rep % 2 else list(ARMS)[::-1]
            for arm in order:
                if f'{case}-{arm}-r{rep}' in done:
                    continue
                samples.append(one(out, case, arm, rep, args.duration_ms,
                                   args.warmup_ms, False))
        for arm in ARMS:
            if f'{case}-{arm}-r1-counters' in done:
                continue
            samples.append(one(out, case, arm, 1, args.duration_ms, args.warmup_ms, True))

    rows = []
    for case in CASES:
        for arm in ARMS:
            group = [s for s in samples if s['case'] == case and s['arm'] == arm
                     and not s['counters_enabled']]
            valid = [s for s in group if s.get('valid')]
            y = [s['sched_yield_entries'] for s in valid]
            t = [s['throughput_ops_per_sec'] for s in valid]
            counter_run = next((s for s in samples if s['case'] == case and s['arm'] == arm
                                and s['counters_enabled']), None)
            rows.append({'case': case, 'arm': arm, 'n_valid': len(valid),
                         'n_invalid': len(group) - len(valid),
                         'sched_yield_entries': y,
                         'mean_sched_yield_entries': statistics.mean(y) if y else None,
                         'throughput_ops_per_sec': t,
                         'mean_throughput_ops_per_sec': statistics.mean(t) if t else None,
                         'counter_run': counter_run['id'] if counter_run else None,
                         'claim_counters': (counter_run or {}).get('claim_counters')})
    for case in CASES:
        pair = {r['arm']: r for r in rows if r['case'] == case}
        for arm in ARMS:
            base = pair['yield']
            r = pair[arm]
            r['yields_relative_to_yield_arm'] = (
                r['mean_sched_yield_entries'] / base['mean_sched_yield_entries']
                if r['mean_sched_yield_entries'] is not None
                and base['mean_sched_yield_entries'] else None)
            r['throughput_relative_to_yield_arm'] = (
                r['mean_throughput_ops_per_sec'] / base['mean_throughput_ops_per_sec']
                if r['mean_throughput_ops_per_sec'] and base['mean_throughput_ops_per_sec']
                else None)
    (out / 'summary.json').write_text(json.dumps(rows, indent=2) + '\n')
    changed = [p for p, h in hashes.items() if b.sha(p) != h]
    meta.update(completed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                inputs_changed=changed, state_after=b.state())
    (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')
    b.fix_owner([out])
    print('SUMMARY ' + json.dumps(rows), flush=True)


if __name__ == '__main__':
    main()
