#!/usr/bin/env python3
"""Sample the NORMAL_DSQ insert-to-switch delay during one readrandom run per arm.

The db_bench process is started exactly as the throughput runner starts it;
bpftrace attaches after the scheduler is loaded and detaches before the run
ends, so the histogram covers a steady-state window inside the run.
"""
import argparse
import datetime
import fcntl
import importlib.util
import json
import os
import re
import shutil
import signal
import subprocess
import tempfile
import time
from pathlib import Path

_spec = importlib.util.spec_from_file_location(
    'user_claim_leveldb', Path(__file__).resolve().with_name('leveldb.py'))
ldb = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ldb)
b = ldb.b

SCRIPT = Path(__file__).resolve().with_name('normal_dsq_latency.bt')


def percentiles(buckets, total):
    """Interpolate p50 and p99 from the linear histogram bucket counts."""
    out = {}
    for name, q in (('p50', 0.5), ('p99', 0.99)):
        target = q * total
        seen = 0
        for lo, hi, count in buckets:
            seen += count
            if seen >= target:
                out[name] = hi if hi is not None else lo
                break
        else:
            out[name] = None
    return out


def sections(text):
    """Split bpftrace output into the bucket lines belonging to each map."""
    current = None
    found = {}
    for line in text.splitlines():
        m = re.match(r'@(\w+):\s*$', line)
        if m:
            current = m[1]
            found.setdefault(current, [])
            continue
        m = re.match(r'\s*\[(\d+), (\d+)\)\s+(\d+)\s', line)
        if m and current:
            found[current].append((int(m[1]), int(m[2]), int(m[3])))
            continue
        m = re.match(r'\s*\[(\d+), \.\.\.\)\s+(\d+)\s', line)
        if m and current:
            found[current].append((int(m[1]), None, int(m[2])))
    return found


def histogram(text, hist_name, max_name, stats_name, over_name):
    buckets = sections(text).get(hist_name, [])
    total = sum(c for _, _, c in buckets)
    result = {'buckets': buckets, 'samples': total}
    result.update(percentiles(buckets, total) if total else {'p50': None, 'p99': None})
    m = re.search(rf'@{max_name}:\s*(\d+)', text)
    result['max_us'] = int(m[1]) if m else None
    m = re.search(rf'@{stats_name}:\s*\{{ \.count = (\d+), \.average = (\d+), \.total = (\d+) \}}', text)
    if m:
        result['count'] = int(m[1])
        result['average_us'] = int(m[2])
    m = re.search(rf'@{over_name}:\s*(\d+)', text)
    result['over_2ms'] = int(m[1]) if m else 0
    return result


def parse(text):
    result = histogram(text, 'wait_us', 'wait_max_us', 'wait_stats', 'over_2ms')
    result['all_tasks'] = histogram(text, 'wait_all_us', 'all_max_us',
                                    'all_stats', 'all_over_2ms')
    for key in ('inserts', 'inserts_all', 'matched', 'matched_all'):
        m = re.search(rf'@{key}:\s*(\d+)', text)
        result[key] = int(m[1]) if m else 0
    return result


def one(out, arm, backend, seed, tmp, trace_seconds, settle_seconds):
    b.wait_disabled()
    cell = ldb.make_cell(arm, backend, False)
    ident = f'readrandom-{b.THREADS}-{cell["id"]}'
    db = tmp / ident
    shutil.copytree(seed, db)
    cmd = [str(b.EXE), f'--threads={b.THREADS}', f'--time_ms={b.TIME_MS}',
           '--benchmarks=readrandom', '--use_existing_db=1', f'--db={db}']
    overrides = b.CONFIG | {'LD_PRELOAD': str(cell['library']),
                            'LD_LIBRARY_PATH': str(cell['libdir'])} | cell['env']
    sample = {'id': ident, 'arm': arm, 'backend': backend, 'command': cmd,
              'environment': overrides, 'trace_seconds': trace_seconds,
              'settle_seconds': settle_seconds, 'seq_before': b.seq()}
    print('START ' + ident, flush=True)
    start = time.monotonic()
    with (out / (ident + '.log')).open('w') as log:
        process = subprocess.Popen(cmd, env=b.ENV | overrides, stdout=log,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            deadline = start + b.TIME_MS / 1000 + 90
            while process.poll() is None and time.monotonic() < deadline:
                if b.state() == 'enabled':
                    break
                time.sleep(0.005)
            sample['scx_enabled'] = b.state() == 'enabled'
            time.sleep(settle_seconds)
            trace = subprocess.Popen(
                ['bpftrace', str(SCRIPT), str(process.pid)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                env=dict(os.environ, BPFTRACE_MAX_MAP_KEYS='65536'))
            time.sleep(trace_seconds)
            trace.send_signal(signal.SIGINT)
            stdout, stderr = trace.communicate(timeout=60)
            (out / (ident + '.bpftrace')).write_text(stdout + '\n--- stderr ---\n' + stderr)
            sample['bpftrace_returncode'] = trace.returncode
            sample['histogram'] = parse(stdout)
            while process.poll() is None and time.monotonic() < deadline:
                time.sleep(0.05)
            sample['timeout'] = process.poll() is None
        finally:
            b.stop(process)
    sample.update(returncode=process.returncode, wall_seconds=time.monotonic() - start,
                  seq_after=b.seq(), state_after=b.state())
    text = (out / (ident + '.log')).read_text()
    total = b.TOTAL_PATTERN.search(text)
    if total:
        sample.update(operations=int(total[1]), roi_seconds=float(total[2]),
                      ops_per_second=float(total[3]))
    shutil.rmtree(db, ignore_errors=True)
    with (out / 'results.jsonl').open('a') as f:
        f.write(json.dumps(sample) + '\n')
    print('DONE ' + json.dumps({k: sample.get(k) for k in
                                ['id', 'ops_per_second', 'histogram']}), flush=True)
    return sample


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--seed', type=Path, default=b.DEFAULT_SEED)
    parser.add_argument('--backend', default='mcs_accordin')
    parser.add_argument('--trace-seconds', type=int, default=15)
    parser.add_argument('--settle-seconds', type=int, default=5)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)

    b.CONFIG = dict(ldb.BASE_CONFIG)
    b.ENV = {k: v for k, v in os.environ.items() if not k.startswith(b.SCRUB)} | b.CONFIG

    guard = os.open(b.LOCK, os.O_RDONLY)
    fcntl.flock(guard, fcntl.LOCK_EX)
    cpus = b.online_cpus()
    b.wait_disabled()
    os.sched_setaffinity(0, cpus)
    tmp = Path(tempfile.mkdtemp(prefix='accordin-user-claim-lat-', dir='/tmp'))
    meta = {'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
            'commit': b.command_output(['git', '-C', str(b.ROOT), 'rev-parse', 'HEAD']),
            'backend': args.backend, 'cpus': cpus, 'uname': list(os.uname()),
            'script_sha256': b.sha(SCRIPT), 'db_bench_sha256': b.sha(b.EXE),
            'trace_seconds': args.trace_seconds, 'settle_seconds': args.settle_seconds}
    (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')
    samples = []
    try:
        for arm in ldb.ARMS:
            samples.append(one(out, arm, args.backend, args.seed, tmp,
                               args.trace_seconds, args.settle_seconds))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
        (out / 'summary.json').write_text(json.dumps(
            [{k: s.get(k) for k in ['id', 'arm', 'backend', 'ops_per_second', 'histogram']}
             for s in samples], indent=2) + '\n')
        meta.update(completed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                    state_after=b.state())
        (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')
        b.fix_owner([out])
    print('SUMMARY ' + json.dumps([{k: s.get(k) for k in
                                    ['id', 'arm', 'ops_per_second', 'histogram']}
                                   for s in samples]), flush=True)


if __name__ == '__main__':
    main()
