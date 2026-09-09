#!/usr/bin/env python3
"""Compare fixed commits of this repository on bench/mutexbench.

Arms are named commits in the form the LevelDB comparison accepts. Worktree
preparation, library selection, environment scrubbing, serialization and the
sched_ext checks come from cv-custody-20260906/run.py, so both comparisons
observe identical conditions. The mutex_bench harness itself is a submodule
binary of this worktree and is shared by every arm; what differs per arm is the
LiTL adapter and the direct library the adapter resolves through
LD_LIBRARY_PATH.

Throughput only: no tracepoint is counted, so the benchmark runs without perf.
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
WORK = ROOT / 'target/bpf-volume-20260909'
BINARY = ROOT / 'bench/mutexbench/mutex_bench'
BACKEND = 'mcs_accordin'

# threads, critical ns, outside ns
CASES = {
    'low-t2-c100-o0': (2, 100, 0),
    'low-t4-c100-o0': (4, 100, 0),
    'low-t4-c100-o3000': (4, 100, 3000),
    'overload-t96-c100-o3000': (96, 100, 3000),
}

METRIC_PATTERN = {
    'throughput_ops_per_sec': re.compile(r'^throughput_ops_per_sec: ([\d.]+)$', re.M),
    'total_operations': re.compile(r'^total_operations: (\d+)$', re.M),
    'elapsed_seconds': re.compile(r'^elapsed_seconds: ([\d.]+)$', re.M),
    'avg_lock_hold_ns': re.compile(r'^avg_lock_hold_ns: ([\d.]+)$', re.M),
}
ERROR_PATTERN = re.compile(
    r'Failed to (?:load|attach|register)|DEBUG DUMP|Assertion.*failed|\[hook_stats\]')


def one(out, case, cell, rep, attempt, cpus, duration_ms, warmup_ms):
    b.wait_disabled()
    threads, critical, outside = CASES[case]
    ident = f'{case}-{cell["id"]}-r{rep}-a{attempt}'
    log_path = out / (ident + '.log')
    cmd = [str(BINARY), '--threads', str(threads), '--duration-ms', str(duration_ms),
           '--warmup-duration-ms', str(warmup_ms), '--critical-ns', str(critical),
           '--outside-ns', str(outside), '--lock-kind', 'mutex']
    overrides = b.CONFIG | {'LD_PRELOAD': str(cell['library']),
                            'LD_LIBRARY_PATH': str(cell['libdir'])} | cell['env']
    limit = duration_ms / 1000.0 + warmup_ms / 1000.0 + 90
    sample = {'id': ident, 'benchmark': case, 'backend': cell['backend'], 'arm': cell['arm'],
              'commit': cell['commit'], 'repeat': rep, 'attempt': attempt,
              'threads': threads, 'critical_ns': critical, 'outside_ns': outside,
              'duration_ms': duration_ms, 'warmup_ms': warmup_ms,
              'command': cmd, 'environment': overrides, 'cpus': cpus,
              'seq_before': b.seq(), 'scx_enabled': False, 'maps': [], 'bpf_fds': [],
              'max_process_threads': 0, 'timeout': False,
              'expected_maps': cell['expected_maps']}
    print('START ' + ident, flush=True)
    kmsg_before = b.dmesg_lines()
    start = time.monotonic()
    with log_path.open('w') as log:
        process = subprocess.Popen(cmd, env=b.ENV | overrides, stdout=log,
                                   stderr=subprocess.STDOUT, start_new_session=True,
                                   cwd=str(BINARY.parent))
        try:
            while process.poll() is None and time.monotonic() - start < limit:
                sample['scx_enabled'] |= b.state() == 'enabled'
                match = re.search(r'^Threads:\s+(\d+)', b.read(f'/proc/{process.pid}/status'), re.M)
                if match:
                    sample['max_process_threads'] = max(sample['max_process_threads'],
                                                        int(match[1]))
                if not sample['maps'] or not sample['bpf_fds']:
                    sample['maps'] = sorted(set(
                        str(Path(line.split()[-1]).resolve())
                        for line in b.read(f'/proc/{process.pid}/maps').splitlines()
                        if any(x in line for x in ('accordin_direct', 'accordin_original'))))
                    for fd in Path(f'/proc/{process.pid}/fdinfo').glob('*'):
                        info = b.read(fd)
                        if re.search(r'^(map_id|prog_id|link_id):', info, re.M):
                            sample['bpf_fds'].append(info)
                time.sleep(0.1 if sample['maps'] and sample['bpf_fds'] else 0.005)
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
    stalls = [line for line in b.dmesg_new(kmsg_before, b.dmesg_lines())
              if any(pattern in line for pattern in b.DMESG_PATTERNS)]
    sample['kernel_stalls'] = stalls

    reasons = []
    if sample['timeout']:
        reasons.append(f'timeout after {limit:.0f}s')
    if process.returncode != 0:
        reasons.append(f'returncode {process.returncode}')
    if 'throughput_ops_per_sec' not in sample:
        reasons.append('no throughput line')
    elif sample['throughput_ops_per_sec'] <= 0:
        reasons.append('non-positive throughput')
    if sample['maps'] != sample['expected_maps']:
        reasons.append(f'maps mismatch {sample["maps"]}')
    if not sample['bpf_fds']:
        reasons.append('no BPF file descriptors')
    if sample['max_process_threads'] < threads:
        reasons.append(f'thread count {sample["max_process_threads"]}')
    if not sample['scx_enabled']:
        reasons.append('sched_ext never enabled')
    if sample['seq_after'] != sample['seq_before'] + 1:
        reasons.append(f'seq {sample["seq_before"]}->{sample["seq_after"]}')
    if stalls:
        reasons.append(f'kernel stall lines: {stalls[0]}')
    error = ERROR_PATTERN.search(text)
    if error:
        reasons.append(f'log error {error[0]!r}')
    sample['valid'] = not reasons
    if reasons:
        sample['reason'] = '; '.join(reasons)
    with (out / 'results.jsonl').open('a') as f:
        f.write(json.dumps(sample) + '\n')
    print('DONE ' + json.dumps({k: sample[k] for k in
                                ['id', 'valid', 'reason', 'throughput_ops_per_sec',
                                 'elapsed_seconds', 'max_process_threads', 'kernel_stalls']
                                if k in sample}), flush=True)
    return sample


def summarize(samples, arms, cases):
    baseline_arm = arms[0]['name']
    valid = [s for s in samples if s.get('valid')]
    rows = []
    for case in cases:
        def values(name):
            return [s['throughput_ops_per_sec'] for s in valid
                    if s['benchmark'] == case and s['arm'] == name
                    and s.get('throughput_ops_per_sec')]
        base = values(baseline_arm)
        base_mean = statistics.mean(base) if base else None
        for arm in arms:
            vals = values(arm['name'])
            n_invalid = sum(1 for s in samples if s['benchmark'] == case
                            and s['arm'] == arm['name'] and not s.get('valid'))
            mean = statistics.mean(vals) if vals else None
            sd = statistics.stdev(vals) if len(vals) > 1 else 0.0
            rows.append({
                'case': case, 'arm': arm['name'], 'commit': arm['resolved_commit'],
                'env': arm['env'], 'n_valid': len(vals), 'n_invalid': n_invalid,
                'runs_ops_per_sec': vals,
                'mean_ops_per_sec': mean, 'stdev': sd if vals else None,
                'cv_percent': 100 * sd / mean if mean else None,
                'relative_to_' + baseline_arm: mean / base_mean if mean and base_mean else None,
            })
    return baseline_arm, rows


def write_markdown(path, baseline_arm, rows, samples):
    lines = ['# mutexbench comparison of fixed commits', '',
             f'Throughput in Kops/s over valid samples only, relative column against '
             f'`{baseline_arm}`.', '',
             '| case | arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% |'
             ' relative | runs |',
             '|---|---|---|---:|---:|---:|---:|---:|---:|---|']
    for r in rows:
        rel = r['relative_to_' + baseline_arm]
        runs = ' / '.join(f'{v / 1000:.1f}' for v in r['runs_ops_per_sec']) or '—'
        mean = r['mean_ops_per_sec']
        lines.append(
            f'| {r["case"]} | {r["arm"]} | `{r["commit"][:7]}` | {r["n_valid"]} | '
            f'{r["n_invalid"]} | '
            + (f'{mean / 1000:.1f}' if mean else '—') + ' | '
            + (f'{r["stdev"] / 1000:.1f}' if mean else '—') + ' | '
            + (f'{r["cv_percent"]:.2f}' if mean else '—') + ' | '
            + (f'{rel:.3f}x' if rel else '—') + f' | {runs} |')
    lines.append('')
    invalid = [s for s in samples if not s.get('valid')]
    lines += ['## Invalid runs', '']
    if invalid:
        lines += ['| run | reason |', '|---|---|']
        lines += [f'| `{s["id"]}` | {s.get("reason", "unknown")} |' for s in invalid]
    else:
        lines.append('None.')
    lines.append('')
    path.write_text('\n'.join(lines) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--arm', action='append', type=b.parse_arm, required=True,
                        metavar='NAME=COMMIT[:ENV=VAL,...]')
    parser.add_argument('--out', type=Path)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--duration-ms', type=int, default=5000)
    parser.add_argument('--warmup-ms', type=int, default=1000)
    parser.add_argument('--cases', default=','.join(CASES))
    parser.add_argument('--backend', default=BACKEND, choices=sorted(b.BACKENDS))
    parser.add_argument('--max-attempts', type=int, default=3,
                        help='attempts per cell before it is left without a valid sample')
    args = parser.parse_args()

    arms = args.arm
    names = [a['name'] for a in arms]
    if len(set(names)) != len(names):
        raise RuntimeError('arm names must be unique')
    cases = [c for c in CASES if c in args.cases.split(',')]
    if not cases:
        raise RuntimeError('no case selected')
    if args.max_attempts < 1:
        raise RuntimeError('--max-attempts must be at least 1')

    stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
    out = (args.out or WORK / ('mutexbench-' + stamp)).resolve()
    out.mkdir(parents=True, exist_ok=True)
    previous = b.load_results(out / 'results.jsonl')
    if previous:
        print(f'RESUME {len(previous)} samples from {out / "results.jsonl"}', flush=True)

    try:
        guard = os.open(b.LOCK, os.O_RDWR | os.O_CREAT, 0o666)
    except OSError:
        guard = os.open(b.LOCK, os.O_RDONLY)
    fcntl.flock(guard, fcntl.LOCK_EX)
    cpus = b.online_cpus()
    b.wait_disabled()
    os.sched_setaffinity(0, cpus)
    if not BINARY.is_file():
        raise RuntimeError(f'missing mutex_bench at {BINARY}')

    sources = {}
    for arm in arms:
        b.prepare_source(arm, sources, out)
    cells = [b.make_cell(arm, args.backend) for arm in arms]

    files = {BINARY, Path(__file__).resolve(),
             Path(__file__).resolve().parents[1] / 'cv-custody-20260906/run.py'}
    for source in sources.values():
        for pattern in ('src/*.c', 'src/*.h', 'src/bpf/*.c', 'src/bpf/*.h', 'include/*.h',
                        'third_party/litl/src/accordin*.c'):
            files.update(source.glob(pattern))
        files.add(source / 'Makefile')
    for cell in cells:
        files.update(Path(p) for p in cell['expected_maps'])
    hashes = {str(p): b.sha(p) for p in sorted(files) if p.is_file()}

    meta = {
        'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'arms': [{'name': a['name'], 'requested': a['commit'], 'commit': a['resolved_commit'],
                  'source': str(a['source']), 'env': a['env']} for a in arms],
        'uname': list(os.uname()),
        'compiler': b.command_output(['clang', '--version']),
        'litl_compiler': b.command_output(['cc', '--version']),
        'harness': {'path': str(BINARY), 'sha256': b.sha(BINARY),
                    'submodule': b.command_output(
                        ['git', '-C', str(ROOT), 'submodule', 'status', 'bench/mutexbench'])},
        'backend': args.backend, 'hashes': hashes, 'configuration': b.CONFIG,
        'cases': {k: CASES[k] for k in cases},
        'cpu_governors': sorted(set(b.read(p) for p in Path(
            '/sys/devices/system/cpu').glob('cpu[0-9]*/cpufreq/scaling_governor'))),
        'cpu0_khz': b.read('/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq'),
        'cpus': cpus, 'cpu_online_spec': b.read('/sys/devices/system/cpu/online'),
        'repeats': args.repeats, 'duration_ms': args.duration_ms,
        'warmup_ms': args.warmup_ms, 'max_attempts': args.max_attempts,
        'resumed_samples': len(previous),
        'dmesg_available': b.dmesg_lines() is not None,
    }
    (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')

    try:
        samples = list(previous)
        exhausted = []
        for case in cases:
            for rep in range(1, args.repeats + 1):
                rotate = (rep - 1) % len(cells)
                order = cells[rotate:] + cells[:rotate]
                for cell in order:
                    done = [s for s in samples if s['benchmark'] == case
                            and s['arm'] == cell['arm'] and s['repeat'] == rep]
                    if any(s.get('valid') for s in done):
                        print(f'SKIP {case}-{cell["id"]}-r{rep} (valid sample recorded)',
                              flush=True)
                        continue
                    for attempt in range(len(done) + 1, args.max_attempts + 1):
                        sample = one(out, case, cell, rep, attempt, cpus,
                                     args.duration_ms, args.warmup_ms)
                        samples.append(sample)
                        if sample['valid']:
                            break
                        print(f'INVALID {sample["id"]}: {sample["reason"]}', flush=True)
                    else:
                        exhausted.append({'case': case, 'arm': cell['arm'], 'repeat': rep,
                                          'attempts': args.max_attempts})
                        print(f'EXHAUSTED {case}-{cell["id"]}-r{rep} after '
                              f'{args.max_attempts} attempts', flush=True)

        changed = [p for p, h in hashes.items() if b.sha(p) != h]
        if changed:
            raise RuntimeError(f'inputs changed during the session: {changed}')
        for source in sources.values():
            dirty = b.command_output(['git', '-C', str(source), 'status', '--porcelain'])
            if dirty:
                raise RuntimeError(f'{source} became dirty:\n{dirty}')

        baseline_arm, rows = summarize(samples, arms, cases)
        invalid = [{'id': s['id'], 'case': s['benchmark'], 'arm': s['arm'],
                    'repeat': s['repeat'], 'attempt': s.get('attempt'),
                    'reason': s.get('reason', 'unknown')}
                   for s in samples if not s.get('valid')]
        (out / 'summary.json').write_text(json.dumps(
            {'baseline_arm': baseline_arm, 'metric': 'throughput_ops_per_sec',
             'lower_is_better': False, 'cells': rows, 'invalid_runs': invalid,
             'cells_without_valid_sample': exhausted}, indent=2) + '\n')
        write_markdown(out / 'summary.md', baseline_arm, rows, samples)
        meta.update(completed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                    valid_runs=sum(1 for s in samples if s.get('valid')),
                    invalid_runs=len(invalid),
                    cells_without_valid_sample=exhausted,
                    hashes_unchanged=True, state_after=b.state())
        (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')
        b.fix_owner([out])
        print('SUMMARY ' + json.dumps(rows), flush=True)
        print('OUTPUT ' + str(out), flush=True)
        if exhausted:
            print('INCOMPLETE ' + json.dumps(exhausted), flush=True)
            return 1
        return 0
    except BaseException:
        b.fix_owner([out])
        raise


if __name__ == '__main__':
    raise SystemExit(main())
