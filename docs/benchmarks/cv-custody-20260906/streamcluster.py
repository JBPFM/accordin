#!/usr/bin/env python3
"""Compare fixed commits of this repository on streamcluster and its barrier.

Arms are named commits, optionally carrying per-arm environment overrides, in
the same form the LevelDB comparison accepts. Worktree preparation, library
selection, environment scrubbing, serialization and sched_ext checks are reused
from run.py so both comparisons observe identical conditions.
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
    'cv_custody_run', Path(__file__).resolve().with_name('run.py'))
b = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(b)

ROOT = b.ROOT
WORK = b.WORK
THREADS = b.THREADS

STREAM = 'target/flexguard-suite-20260905/streamcluster/streamcluster'
BARRIER = 'target/streamcluster-analysis-20260905/barrier'
BINARY_ROOTS = [ROOT, Path('/mnt/data/home/jz/accordin-m0')]

# The clustering output is deterministic for a given machine architecture but
# not across architectures: the aarch64 reference digest and the x86_64 digest
# observed on this host both denote a correct run. --expected-sha pins one.
ACCEPTED_STREAM_SHA = {
    'dfeea2357203cceeb8bcdac4984ffc9da9c953f1f1d19c06990626a4575ef01f': 'aarch64 reference',
    '13b33997906352993b3d67c0d86cf2a984741f96163597990e935d3f72cc5b84': 'x86_64 reference',
}

MODES = ['stream', 'barrier']
TIMEOUT = {'stream': 900, 'barrier': 60}
SECONDS_PATTERN = {'stream': re.compile(r'Benchmark time:\s*(\d+)'),
                   'barrier': re.compile(r'seconds=([\d.]+)')}
SECONDS_SCALE = {'stream': 1e8, 'barrier': 1.0}
ERROR_PATTERN = re.compile(
    r'Failed to (?:load|attach|register)|DEBUG DUMP|Assertion.*failed|\[hook_stats\]')


def locate(relative):
    """Find a prebuilt benchmark binary in this worktree or its sibling."""
    for root in BINARY_ROOTS:
        candidate = root / relative
        if candidate.is_file():
            return candidate
    raise RuntimeError(f'missing benchmark binary {relative} under '
                       + ', '.join(str(r) for r in BINARY_ROOTS))


def command(mode, binaries, output):
    if mode == 'barrier':
        return [str(binaries['barrier']), str(THREADS), '100']
    return [str(binaries['stream']), '10', '30', '512', '32768', '32768', '2000',
            'none', str(output), str(THREADS)]


def run(out, mode, cell, rep, attempt, cpus, binaries, accepted):
    b.wait_disabled()
    ident = f'{mode}-{THREADS}-{cell["id"]}-r{rep}-a{attempt}'
    output = out / (ident + '.clusters')
    cmd = command(mode, binaries, output)
    overrides = b.CONFIG | {'LD_PRELOAD': str(cell['library']),
                            'LD_LIBRARY_PATH': str(cell['libdir'])} | cell['env']
    limit = TIMEOUT[mode]
    sample = {'id': ident, 'benchmark': mode, 'arm': cell['arm'], 'backend': cell['backend'],
              'commit': cell['commit'], 'repeat': rep, 'attempt': attempt, 'command': cmd,
              'environment': overrides, 'threads': THREADS, 'cpus': cpus,
              'seq_before': b.seq(), 'scx_enabled': False, 'maps': [], 'bpf_fds': [],
              'max_process_threads': 0, 'timeout': False, 'expected_maps': cell['expected_maps']}
    print('START ' + ident, flush=True)
    kmsg_before = b.dmesg_lines()
    start = time.monotonic()
    with (out / (ident + '.log')).open('w') as log:
        process = subprocess.Popen(cmd, env=b.ENV | overrides, stdout=log,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            while process.poll() is None and time.monotonic() - start < limit:
                sample['scx_enabled'] |= b.state() == 'enabled'
                match = re.search(r'^Threads:\s+(\d+)', b.read(f'/proc/{process.pid}/status'), re.M)
                if match:
                    sample['max_process_threads'] = max(sample['max_process_threads'], int(match[1]))
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
    text = (out / (ident + '.log')).read_text()
    match = SECONDS_PATTERN[mode].search(text)
    seconds = float(match[1]) / SECONDS_SCALE[mode] if match else None
    sample['seconds'] = seconds
    counters = b.COUNTER_PATTERN.findall(text)
    if counters:
        sample['counters'] = {k: int(v) for k, v in zip(b.COUNTER_KEYS, counters[-1])}
    stalls = [line for line in b.dmesg_new(kmsg_before, b.dmesg_lines())
              if any(pattern in line for pattern in b.DMESG_PATTERNS)]
    sample['kernel_stalls'] = stalls

    reasons = []
    if sample['timeout']:
        reasons.append(f'timeout after {limit}s')
    if process.returncode != 0:
        reasons.append(f'returncode {process.returncode}')
    if seconds is None:
        reasons.append('no benchmark time line')
    elif seconds <= 0:
        reasons.append(f'non-positive time {seconds}')
    if mode == 'stream':
        sample['output_sha256'] = b.sha(output) if output.exists() else None
        if sample['output_sha256'] not in accepted:
            reasons.append(f'clustering output sha256 {sample["output_sha256"]}')
        if sample['max_process_threads'] < THREADS + 1:
            reasons.append(f'thread count {sample["max_process_threads"]}')
    if sample['maps'] != sample['expected_maps']:
        reasons.append(f'maps mismatch {sample["maps"]}')
    if not sample['bpf_fds']:
        reasons.append('no BPF file descriptors')
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
    reported = {k: sample[k] for k in
                ['id', 'valid', 'reason', 'seconds', 'output_sha256', 'max_process_threads',
                 'counters', 'kernel_stalls'] if k in sample}
    print('DONE ' + json.dumps(reported), flush=True)
    return sample


def summarize(samples, arms, modes, backends):
    """Aggregate valid samples per mode x backend x arm, counting invalid attempts."""
    baseline_arm = arms[0]['name']
    valid = [s for s in samples if s.get('valid')]
    rows = []
    for mode in modes:
        for backend in backends:
            def belongs(s, name):
                return (s['benchmark'] == mode and s['backend'] == backend and s['arm'] == name)

            def values(name):
                return [s['seconds'] for s in valid if belongs(s, name) and s.get('seconds')]
            base = values(baseline_arm)
            base_mean = statistics.mean(base) if base else None
            for arm in arms:
                vals = values(arm['name'])
                n_invalid = sum(1 for s in samples
                                if belongs(s, arm['name']) and not s.get('valid'))
                if not vals and not n_invalid:
                    continue
                mean = statistics.mean(vals) if vals else None
                sd = statistics.stdev(vals) if len(vals) > 1 else 0.0
                counters = [s['counters'] for s in valid
                            if belongs(s, arm['name']) and 'counters' in s]
                rows.append({
                    'benchmark': mode, 'backend': backend, 'arm': arm['name'],
                    'commit': arm['resolved_commit'], 'env': arm['env'],
                    'n_valid': len(vals), 'n_invalid': n_invalid,
                    'mean_seconds': mean, 'stdev': sd if vals else None,
                    'cv_percent': 100 * sd / mean if mean else None,
                    'relative_to_' + baseline_arm: mean / base_mean if mean and base_mean else None,
                    'runs_seconds': vals,
                    'mean_counters': {k: statistics.mean([c[k] for c in counters])
                                      for k in b.COUNTER_KEYS} if counters else None,
                })
    return baseline_arm, rows


def write_markdown(path, baseline_arm, rows, samples):
    lines = ['# streamcluster comparison of fixed commits', '',
             f'Seconds over valid samples only, lower is better; the relative column is the '
             f'ratio of mean seconds against `{baseline_arm}`, so below 1.000 is faster.', '']
    for mode in dict.fromkeys(r['benchmark'] for r in rows):
        for backend in dict.fromkeys(r['backend'] for r in rows if r['benchmark'] == mode):
            cell = [r for r in rows if r['benchmark'] == mode and r['backend'] == backend]
            lines += [f'## {mode} / {backend}', '',
                      '| arm | commit | n valid | n invalid | mean s | stdev | CV% |'
                      ' relative | runs |',
                      '|---|---|---:|---:|---:|---:|---:|---:|---|']
            for r in cell:
                rel = r['relative_to_' + baseline_arm]
                runs = ' / '.join(f'{v:.3f}' for v in r['runs_seconds']) or '—'
                mean = r['mean_seconds']
                lines.append(
                    f'| {r["arm"]} | `{r["commit"][:7]}` | {r["n_valid"]} | {r["n_invalid"]} | '
                    + (f'{mean:.3f}' if mean else '—') + ' | '
                    + (f'{r["stdev"]:.3f}' if mean else '—') + ' | '
                    + (f'{r["cv_percent"]:.2f}' if mean else '—') + ' | '
                    + (f'{rel:.3f}x' if rel else '—') + f' | {runs} |')
            lines.append('')
            if any(r['mean_counters'] for r in cell):
                lines += ['| arm | ' + ' | '.join(b.COUNTER_KEYS) + ' |',
                          '|---|' + '---:|' * len(b.COUNTER_KEYS)]
                for r in cell:
                    if r['mean_counters']:
                        lines.append(f'| {r["arm"]} | ' + ' | '.join(
                            f'{r["mean_counters"][k]:.1f}' for k in b.COUNTER_KEYS) + ' |')
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
    parser.add_argument('--modes', nargs='+', default=MODES, choices=MODES)
    parser.add_argument('--backends', default=','.join(b.BACKENDS))
    parser.add_argument('--expected-sha', action='append',
                        help='accepted clustering output digest; repeatable')
    parser.add_argument('--max-attempts', type=int, default=3,
                        help='attempts per cell before it is left without a valid sample')
    args = parser.parse_args()

    arms = args.arm
    names = [a['name'] for a in arms]
    if len(set(names)) != len(names):
        raise RuntimeError('arm names must be unique')
    modes = [m for m in MODES if m in args.modes]
    backends = [x for x in b.BACKENDS if x in args.backends.split(',')]
    if not modes or not backends:
        raise RuntimeError('no mode or backend selected')
    if args.max_attempts < 1:
        raise RuntimeError('--max-attempts must be at least 1')
    accepted = set(args.expected_sha) if args.expected_sha else set(ACCEPTED_STREAM_SHA)

    stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
    out = (args.out or WORK / ('streamcluster-' + stamp)).resolve()
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
    binaries = {'stream': locate(STREAM), 'barrier': locate(BARRIER)}

    sources = {}
    for arm in arms:
        b.prepare_source(arm, sources, out)
    cells = [b.make_cell(arm, backend) for arm in arms for backend in backends]

    files = set(binaries.values()) | {Path(__file__).resolve(),
                                      Path(__file__).resolve().with_name('run.py')}
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
        'benchmark_binaries': {k: {'path': str(v), 'sha256': b.sha(v)}
                               for k, v in binaries.items()},
        'accepted_stream_output_sha256': sorted(accepted),
        'hashes': hashes, 'configuration': b.CONFIG,
        'cpu_governors': sorted(set(b.read(p) for p in
                                    Path('/sys/devices/system/cpu').glob('cpu[0-9]*/cpufreq/scaling_governor'))),
        'cpu0_khz': b.read('/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq'),
        'threads': THREADS, 'cpus': cpus,
        'cpu_online_spec': b.read('/sys/devices/system/cpu/online'),
        'timeouts_seconds': TIMEOUT, 'repeats': args.repeats,
        'max_attempts': args.max_attempts, 'resumed_samples': len(previous),
        'modes': modes, 'backends': backends,
        'dmesg_available': b.dmesg_lines() is not None,
    }
    (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')

    try:
        samples = list(previous)
        exhausted = []
        for rep in range(1, args.repeats + 1):
            rotate = (rep - 1) % len(arms)
            order = [c for arm in arms[rotate:] + arms[:rotate] for c in cells
                     if c['arm'] == arm['name']]
            for mode in modes:
                for cell in order:
                    key = (mode, cell['backend'], cell['arm'], rep)
                    done = [s for s in samples if b.cell_key(s) == key]
                    if any(s.get('valid') for s in done):
                        print(f'SKIP {mode}-{cell["id"]}-r{rep} (valid sample recorded)',
                              flush=True)
                        continue
                    for attempt in range(len(done) + 1, args.max_attempts + 1):
                        sample = run(out, mode, cell, rep, attempt, cpus, binaries, accepted)
                        samples.append(sample)
                        if sample['valid']:
                            break
                        print(f'INVALID {sample["id"]}: {sample["reason"]}', flush=True)
                    else:
                        exhausted.append({'benchmark': mode, 'backend': cell['backend'],
                                          'arm': cell['arm'], 'repeat': rep,
                                          'attempts': args.max_attempts})
                        print(f'EXHAUSTED {mode}-{cell["id"]}-r{rep} after '
                              f'{args.max_attempts} attempts', flush=True)

        changed = [p for p, h in hashes.items() if b.sha(p) != h]
        if changed:
            raise RuntimeError(f'inputs changed during the session: {changed}')
        for source in sources.values():
            dirty = b.command_output(['git', '-C', str(source), 'status', '--porcelain'])
            if dirty:
                raise RuntimeError(f'{source} became dirty:\n{dirty}')

        baseline_arm, rows = summarize(samples, arms, modes, backends)
        invalid = [{'id': s['id'], 'benchmark': s['benchmark'], 'backend': s['backend'],
                    'arm': s['arm'], 'repeat': s['repeat'], 'attempt': s.get('attempt'),
                    'reason': s.get('reason', 'unknown')}
                   for s in samples if not s.get('valid')]
        (out / 'summary.json').write_text(json.dumps(
            {'baseline_arm': baseline_arm, 'metric': 'seconds', 'lower_is_better': True,
             'cells': rows, 'invalid_runs': invalid,
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
