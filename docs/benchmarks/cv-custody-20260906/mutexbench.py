#!/usr/bin/env python3
"""Compare fixed commits of this repository on mutexbench direct locks.

Arms are named commits, optionally carrying per-arm environment overrides, in
the same form the LevelDB comparison accepts. Worktree preparation, library
selection, environment scrubbing, serialization and sched_ext checks are reused
from run.py so all three comparisons observe identical conditions.

mutex_bench takes no LiTL adapter: it dlopens the direct library named by
MCS_ACCORDIN_DIRECT_LIB or MCS_TAS_ACCORDIN_DIRECT_LIB, so an arm is selected
purely by pointing those variables at its own target/release. One binary built
from this worktree's submodule serves every arm.

Two workload shapes are available. The single-lock shape sweeps one contended
lock over critical/outside nanosecond pairs and thread counts, reporting
throughput and how evenly the threads shared the lock. The two-lock shape
splits the threads into two equal groups, each hammering its own lock with its
own timing, and reports how the machine is shared between the two groups
alongside total throughput.
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
BINARY = ROOT / 'bench/mutexbench/mutex_bench'

BACKENDS = ['mcs_accordin_direct', 'mcs_tas_accordin_direct']
DEFAULT_PAIRS = ['100:3000', '300:3000']
DURATION_MS = 5000
WARMUP_MS = 1000

# Two groups of threads, each on its own lock, named by the critical and
# outside nanoseconds given to group A and to group B.
TWO_LOCK_CASES = {
    'homogeneous': (300, 3000, 300, 3000),
    'heterogeneous_mild': (3000, 3000, 300, 3000),
    'heterogeneous_extreme': (3000, 300, 100, 3000),
}
DEFAULT_CASES = list(TWO_LOCK_CASES)

GROUP_METRICS = ['throughput_ops_per_sec', 'normalized_efficiency',
                 'normalized_slowdown', 'avg_wait_ns_estimated']
METRIC_KEYS = {
    'single': ['throughput_ops_per_sec', 'avg_lock_hold_ns', 'avg_wait_ns_estimated'],
    'two-lock': ['throughput_ops_per_sec', 'fairness_jain']
                + [f'group_{group}_{key}' for group in ('a', 'b') for key in GROUP_METRICS],
}
METRIC_PATTERN = {k: re.compile(rf'^{k}: *([-+0-9.eE]+) *$', re.M)
                  for keys in METRIC_KEYS.values() for k in keys}
# Derived from the per-thread operation counters of a single-lock run rather
# than from a line of its own.
PER_THREAD_KEYS = ['fairness_factor', 'per_thread_min_operations',
                   'per_thread_max_operations', 'per_thread_mean_operations']
PER_THREAD_PATTERN = re.compile(r'^per_thread_operations: *(.*[0-9].*?) *$', re.M)
LOADED_MARKER = 'eBPF scheduler loaded successfully'
ERROR_PATTERN = re.compile(
    r'Failed to (?:load|attach|register)|DEBUG DUMP|EXIT:|Assertion.*failed|\[hook_stats\]')


def parse_pair(spec):
    """Read a CRITICAL:OUTSIDE nanosecond pair naming one single-lock point."""
    critical, sep, outside = spec.partition(':')
    if not sep:
        raise argparse.ArgumentTypeError(f'expected CRITICAL_NS:OUTSIDE_NS, got {spec!r}')
    try:
        values = (int(critical), int(outside))
    except ValueError:
        raise argparse.ArgumentTypeError(f'expected integers, got {spec!r}') from None
    if min(values) < 0:
        raise argparse.ArgumentTypeError(f'nanoseconds must not be negative: {spec!r}')
    return {'label': f'{values[0]}ns-{values[1]}ns', 'workload': 'single',
            'fields': {'critical_ns': values[0], 'outside_ns': values[1]},
            'args': ['--workload', 'single',
                     '--critical-ns', str(values[0]), '--outside-ns', str(values[1])]}


def parse_case(name):
    """Read a named two-lock case into one measured point."""
    if name not in TWO_LOCK_CASES:
        raise argparse.ArgumentTypeError(
            f'unknown case {name!r}, expected one of {", ".join(TWO_LOCK_CASES)}')
    a_critical, a_outside, b_critical, b_outside = TWO_LOCK_CASES[name]
    return {'label': f'two-lock-{name}', 'workload': 'two-lock',
            'fields': {'case': name, 'group_a_critical_ns': a_critical,
                       'group_a_outside_ns': a_outside, 'group_b_critical_ns': b_critical,
                       'group_b_outside_ns': b_outside},
            'args': ['--workload', 'two-lock',
                     '--group-a-critical-ns', str(a_critical),
                     '--group-a-outside-ns', str(a_outside),
                     '--group-b-critical-ns', str(b_critical),
                     '--group-b-outside-ns', str(b_outside)]}


def with_threads(point, threads, per_thread_required):
    """Bind one workload point to a thread count, which becomes part of its label."""
    return point | {'label': f'{point["label"]}-t{threads}', 'threads': threads,
                    'per_thread_required': per_thread_required,
                    'fields': point['fields'] | {'threads': threads}}


def metric_keys(point):
    """Metrics aggregated for a point: parsed lines plus per-thread derivations."""
    return METRIC_KEYS[point['workload']] + (
        PER_THREAD_KEYS if point['workload'] == 'single' else [])


def per_thread_operations(text):
    """Read the per-thread operation counters printed at the end of a run."""
    found = PER_THREAD_PATTERN.findall(text)
    if not found:
        return None
    return [int(value) for value in found[-1].replace(',', ' ').split()]


def fairness(ops):
    """Describe how evenly the threads shared the lock.

    The fairness factor is the share of all operations taken by the busier half
    of the threads: 0.5 when every thread got the same turn count and 1.0 when
    half the threads never acquired the lock.
    """
    total = sum(ops)
    return {'fairness_factor': sum(sorted(ops, reverse=True)[:len(ops) // 2]) / total
            if total else 0.0,
            'per_thread_min_operations': min(ops),
            'per_thread_max_operations': max(ops),
            'per_thread_mean_operations': statistics.mean(ops)}


def make_cell(arm, backend):
    """Bind an arm to a backend through the direct library mutex_bench dlopens."""
    libdir = arm['source'] / 'target/release'
    direct = libdir / f'lib{backend}.so'
    if not direct.is_file():
        raise RuntimeError(f'missing library {direct}')
    return {
        'id': f'{arm["name"]}-{backend}',
        'arm': arm['name'],
        'backend': backend,
        'commit': arm['resolved_commit'],
        'libdir': libdir,
        'env': arm['env'],
        'expected_maps': [str(direct.resolve())],
    }


def library_environment(libdir):
    return {f'{backend.upper()}_LIB': str(libdir / f'lib{backend}.so')
            for backend in BACKENDS}


def run(out, point, cell, rep, attempt, cpus, duration_ms, warmup_ms):
    b.wait_disabled()
    threads = point['threads']
    metrics = METRIC_KEYS[point['workload']]
    ident = f'{point["label"]}-{cell["id"]}-r{rep}-a{attempt}'
    cmd = [str(BINARY), '--lock-kind', cell['backend'], '--threads', str(threads),
           '--duration-ms', str(duration_ms), '--warmup-duration-ms', str(warmup_ms),
           *point['args'], '--timeslice-extension', 'off']
    overrides = (b.CONFIG | library_environment(cell['libdir'])
                 | {'LD_LIBRARY_PATH': str(cell['libdir'])} | cell['env'])
    limit = (duration_ms + warmup_ms) / 1000.0 + 90
    sample = {'id': ident, 'benchmark': point['label'], 'workload': point['workload'],
              **point['fields'],
              'arm': cell['arm'], 'backend': cell['backend'],
              'commit': cell['commit'], 'repeat': rep, 'attempt': attempt, 'command': cmd,
              'environment': overrides, 'threads': threads, 'cpus': cpus,
              'duration_ms': duration_ms, 'warmup_ms': warmup_ms,
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
    for key in metrics:
        found = METRIC_PATTERN[key].findall(text)
        if found:
            sample[key] = float(found[-1])
    if point['workload'] == 'single':
        ops = per_thread_operations(text)
        if ops:
            sample['per_thread_operations'] = ops
            sample.update(fairness(ops))
    counters = b.COUNTER_PATTERN.findall(text)
    if counters:
        sample['counters'] = {k: int(v) for k, v in zip(b.COUNTER_KEYS, counters[-1])}
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
        reasons.append(f'non-positive throughput {sample["throughput_ops_per_sec"]}')
    if point.get('per_thread_required'):
        recorded = len(sample.get('per_thread_operations', []))
        if recorded != threads:
            reasons.append(f'per-thread operations for {recorded} of {threads} threads')
    if point['workload'] == 'two-lock':
        for group in ('a', 'b'):
            key = f'group_{group}_throughput_ops_per_sec'
            if sample.get(key, 0.0) <= 0:
                reasons.append(f'group {group} produced no operations')
        if 'fairness_jain' not in sample:
            reasons.append('no fairness line')
    if sample['maps'] != sample['expected_maps']:
        reasons.append(f'maps mismatch {sample["maps"]}')
    if not sample['bpf_fds']:
        reasons.append('no BPF file descriptors')
    if sample['max_process_threads'] < threads + 1:
        reasons.append(f'thread count {sample["max_process_threads"]}')
    if not sample['scx_enabled']:
        reasons.append('sched_ext never enabled')
    if sample['seq_after'] != sample['seq_before'] + 1:
        reasons.append(f'seq {sample["seq_before"]}->{sample["seq_after"]}')
    if sample['state_after'] != 'disabled':
        reasons.append(f'state after run {sample["state_after"]!r}')
    if LOADED_MARKER not in text:
        reasons.append('scheduler load message missing')
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
                ['id', 'valid', 'reason', *metric_keys(point), 'max_process_threads',
                 'counters', 'kernel_stalls'] if k in sample}
    print('DONE ' + json.dumps(reported), flush=True)
    return sample


def summarize(samples, arms, points, backends):
    """Aggregate valid samples per workload point x backend x arm, counting invalid attempts."""
    baseline_arm = arms[0]['name']
    valid = [s for s in samples if s.get('valid')]
    rows = []
    for point in points:
        metrics = metric_keys(point)
        for backend in backends:
            def belongs(s, name):
                return (s['benchmark'] == point['label'] and s['backend'] == backend
                        and s['arm'] == name)

            def values(name, key='throughput_ops_per_sec'):
                return [s[key] for s in valid if belongs(s, name) and s.get(key) is not None]
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
                runs = {k: values(arm['name'], k) for k in metrics[1:]}
                rows.append({
                    'benchmark': point['label'], 'workload': point['workload'],
                    **point['fields'],
                    'backend': backend, 'arm': arm['name'],
                    'commit': arm['resolved_commit'], 'env': arm['env'],
                    'n_valid': len(vals), 'n_invalid': n_invalid,
                    'mean_ops_per_second': mean, 'stdev': sd if vals else None,
                    'cv_percent': 100 * sd / mean if mean else None,
                    'relative_to_' + baseline_arm: mean / base_mean if mean and base_mean else None,
                    'runs_ops_per_second': vals,
                    'mean_metrics': {k: statistics.mean(v) if v else None
                                     for k, v in runs.items()},
                    'runs_metrics': runs,
                    'mean_counters': {k: statistics.mean([c[k] for c in counters])
                                      for k in b.COUNTER_KEYS} if counters else None,
                })
    return baseline_arm, rows


def cell_rows(rows, label, backend):
    return [r for r in rows if r['benchmark'] == label and r['backend'] == backend]


def number(value, scale=1.0, digits=3):
    return f'{value / scale:.{digits}f}' if value is not None else '—'


def single_tables(lines, cell, baseline_arm):
    lines += ['| arm | commit | n valid | n invalid | mean Mops/s | stdev | CV% |'
              ' relative | runs |',
              '|---|---|---:|---:|---:|---:|---:|---:|---|']
    for r in cell:
        rel = r['relative_to_' + baseline_arm]
        runs = ' / '.join(f'{v / 1e6:.3f}' for v in r['runs_ops_per_second']) or '—'
        mean = r['mean_ops_per_second']
        lines.append(
            f'| {r["arm"]} | `{r["commit"][:7]}` | {r["n_valid"]} | {r["n_invalid"]} | '
            + number(mean, 1e6) + ' | ' + (number(r['stdev'], 1e6) if mean else '—')
            + ' | ' + (f'{r["cv_percent"]:.2f}' if mean else '—') + ' | '
            + (f'{rel:.3f}x' if rel else '—') + f' | {runs} |')
    lines += ['', '| arm | mean lock hold ns | mean wait ns estimated |', '|---|---:|---:|']
    for r in cell:
        held, waited = (r['mean_metrics'][k] for k in
                        ['avg_lock_hold_ns', 'avg_wait_ns_estimated'])
        lines.append(f'| {r["arm"]} | ' + number(held, 1, 1) + ' | ' + number(waited, 1, 1) + ' |')
    lines.append('')
    fairness_table(lines, cell)


def fairness_table(lines, cell):
    """Per-thread spread of a single-lock point, absent from runs predating it."""
    if not any(r['runs_metrics'].get('fairness_factor') for r in cell):
        return
    lines += ['| arm | fairness mean | fairness min | fairness max |'
              ' mean per-thread min ops | mean per-thread max ops | runs fairness |',
              '|---|---:|---:|---:|---:|---:|---|']
    for r in cell:
        runs = r['runs_metrics'].get('fairness_factor') or []
        lines.append(
            f'| {r["arm"]} | ' + number(r['mean_metrics'].get('fairness_factor'), 1, 4) + ' | '
            + (f'{min(runs):.4f}' if runs else '—') + ' | '
            + (f'{max(runs):.4f}' if runs else '—') + ' | '
            + number(r['mean_metrics'].get('per_thread_min_operations'), 1, 0) + ' | '
            + number(r['mean_metrics'].get('per_thread_max_operations'), 1, 0) + ' | '
            + (' / '.join(f'{v:.4f}' for v in runs) or '—') + ' |')
    lines.append('')


def two_lock_tables(lines, cell, baseline_arm):
    lines += ['| arm | commit | n valid | n invalid | mean total Mops/s | CV% | relative |'
              ' Jain mean | Jain min | Jain max | A Mops/s | B Mops/s | A slowdown |'
              ' B slowdown | runs Jain |',
              '|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|']
    for r in cell:
        rel = r['relative_to_' + baseline_arm]
        mean = r['mean_ops_per_second']
        jain = r['runs_metrics']['fairness_jain']
        lines.append(
            f'| {r["arm"]} | `{r["commit"][:7]}` | {r["n_valid"]} | {r["n_invalid"]} | '
            + number(mean, 1e6) + ' | ' + (f'{r["cv_percent"]:.2f}' if mean else '—') + ' | '
            + (f'{rel:.3f}x' if rel else '—') + ' | '
            + number(r['mean_metrics']['fairness_jain'], 1, 4) + ' | '
            + (f'{min(jain):.4f}' if jain else '—') + ' | '
            + (f'{max(jain):.4f}' if jain else '—') + ' | '
            + number(r['mean_metrics']['group_a_throughput_ops_per_sec'], 1e6) + ' | '
            + number(r['mean_metrics']['group_b_throughput_ops_per_sec'], 1e6) + ' | '
            + number(r['mean_metrics']['group_a_normalized_slowdown'], 1, 2) + ' | '
            + number(r['mean_metrics']['group_b_normalized_slowdown'], 1, 2) + ' | '
            + (' / '.join(f'{v:.4f}' for v in jain) or '—') + ' |')
    lines.append('')


def write_markdown(path, baseline_arm, rows, samples):
    lines = ['# mutexbench direct-lock comparison of fixed commits', '',
             f'Throughput in Mops/s over valid samples only, higher is better; the relative '
             f'column is the ratio of mean throughput against `{baseline_arm}`, so above 1.000 '
             f'is faster. Each section is one measured workload point.', '']
    for label in dict.fromkeys(r['benchmark'] for r in rows):
        for backend in dict.fromkeys(r['backend'] for r in rows if r['benchmark'] == label):
            cell = cell_rows(rows, label, backend)
            lines += [f'## {label} / {backend}', '']
            if cell[0]['workload'] == 'two-lock':
                two_lock_tables(lines, cell, baseline_arm)
            else:
                single_tables(lines, cell, baseline_arm)
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
    parser.add_argument('--workload', choices=sorted(METRIC_KEYS), default='single')
    parser.add_argument('--pairs', nargs='+', type=parse_pair, default=None,
                        metavar='CRITICAL_NS:OUTSIDE_NS',
                        help='single-lock workload points')
    parser.add_argument('--cases', nargs='+', type=parse_case, default=None,
                        metavar='CASE', help='two-lock cases: ' + ', '.join(TWO_LOCK_CASES))
    parser.add_argument('--threads', nargs='+', type=int, default=[b.THREADS],
                        metavar='COUNT', help='thread counts, each measured separately')
    parser.add_argument('--per-thread', action='store_true',
                        help='require the per-thread operation line of a single-lock run')
    parser.add_argument('--duration-ms', type=int, default=DURATION_MS)
    parser.add_argument('--warmup-ms', type=int, default=WARMUP_MS)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--backends', default=','.join(BACKENDS))
    parser.add_argument('--max-attempts', type=int, default=3,
                        help='attempts per cell before it is left without a valid sample')
    args = parser.parse_args()

    arms = args.arm
    names = [a['name'] for a in arms]
    if len(set(names)) != len(names):
        raise RuntimeError('arm names must be unique')
    if args.workload == 'two-lock':
        if args.pairs is not None:
            raise RuntimeError('--pairs belongs to the single-lock workload')
        if args.per_thread:
            raise RuntimeError('--per-thread belongs to the single-lock workload')
        shapes = args.cases if args.cases is not None else [parse_case(c) for c in DEFAULT_CASES]
        if any(t % 2 for t in args.threads):
            raise RuntimeError('the two-lock workload needs even --threads values')
    else:
        if args.cases is not None:
            raise RuntimeError('--cases belongs to the two-lock workload')
        shapes = args.pairs if args.pairs is not None else [parse_pair(p) for p in DEFAULT_PAIRS]
    threads = list(dict.fromkeys(args.threads))
    points = [with_threads(shape, count, args.per_thread)
              for shape in shapes for count in threads]
    backends = [x for x in BACKENDS if x in args.backends.split(',')]
    if not points or not backends:
        raise RuntimeError('no workload point or backend selected')
    if min(*threads, args.duration_ms, args.repeats) < 1 or args.warmup_ms < 0:
        raise RuntimeError('threads, duration and repeats must be positive')
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
        raise RuntimeError(f'missing mutex_bench at {BINARY}; build it with '
                           f'make -C bench/mutexbench mutex_bench')

    sources = {}
    for arm in arms:
        b.prepare_source(arm, sources, out)
    cells = [make_cell(arm, backend) for arm in arms for backend in backends]

    files = {BINARY, Path(__file__).resolve(), Path(__file__).resolve().with_name('run.py')}
    for source in sources.values():
        for pattern in ('src/*.c', 'src/*.h', 'src/bpf/*.c', 'src/bpf/*.h', 'include/*.h'):
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
        'benchmark_compiler': b.command_output(['g++', '--version']),
        'benchmark_binary': {'path': str(BINARY), 'sha256': b.sha(BINARY),
                             'submodule_commit': b.command_output(
                                 ['git', '-C', str(BINARY.parent), 'rev-parse', 'HEAD'])},
        'hashes': hashes, 'configuration': b.CONFIG,
        'library_environment': {k: '<arm>/target/release/' + Path(v).name
                                for k, v in library_environment(Path('.')).items()},
        'cpu_governors': sorted(set(b.read(p) for p in
                                    Path('/sys/devices/system/cpu').glob('cpu[0-9]*/cpufreq/scaling_governor'))),
        'cpu0_khz': b.read('/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq'),
        'threads': threads, 'cpus': cpus,
        'cpu_online_spec': b.read('/sys/devices/system/cpu/online'),
        'duration_ms': args.duration_ms, 'warmup_ms': args.warmup_ms,
        'workload': args.workload, 'points': points, 'repeats': args.repeats,
        'max_attempts': args.max_attempts, 'resumed_samples': len(previous),
        'backends': backends,
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
            for point in points:
                for cell in order:
                    key = (point['label'], cell['backend'], cell['arm'], rep)
                    done = [s for s in samples if b.cell_key(s) == key]
                    if any(s.get('valid') for s in done):
                        print(f'SKIP {point["label"]}-{cell["id"]}-r{rep} (valid sample recorded)',
                              flush=True)
                        continue
                    for attempt in range(len(done) + 1, args.max_attempts + 1):
                        sample = run(out, point, cell, rep, attempt, cpus,
                                     args.duration_ms, args.warmup_ms)
                        samples.append(sample)
                        if sample['valid']:
                            break
                        print(f'INVALID {sample["id"]}: {sample["reason"]}', flush=True)
                    else:
                        exhausted.append({'benchmark': point['label'], 'backend': cell['backend'],
                                          'arm': cell['arm'], 'repeat': rep,
                                          'attempts': args.max_attempts})
                        print(f'EXHAUSTED {point["label"]}-{cell["id"]}-r{rep} after '
                              f'{args.max_attempts} attempts', flush=True)

        changed = [p for p, h in hashes.items() if b.sha(p) != h]
        if changed:
            raise RuntimeError(f'inputs changed during the session: {changed}')
        for source in sources.values():
            dirty = b.command_output(['git', '-C', str(source), 'status', '--porcelain'])
            if dirty:
                raise RuntimeError(f'{source} became dirty:\n{dirty}')

        baseline_arm, rows = summarize(samples, arms, points, backends)
        invalid = [{'id': s['id'], 'benchmark': s['benchmark'], 'backend': s['backend'],
                    'arm': s['arm'], 'repeat': s['repeat'], 'attempt': s.get('attempt'),
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
