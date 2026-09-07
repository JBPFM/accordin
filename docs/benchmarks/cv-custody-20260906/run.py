#!/usr/bin/env python3
"""Compare fixed commits of this repository on LevelDB db_bench.

Arms are named commits, optionally carrying per-arm environment overrides.
Each distinct commit is built in its own detached worktree and measured with
the LiTL original adapter preloaded against that worktree's direct library.
"""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
import re
import shutil
import signal
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

ROOT = next(p for p in Path(__file__).resolve().parents if (p / 'src/direct.c').is_file())
WORK = ROOT / 'target/cv-custody-20260906'
EXE = Path('/mnt/data/home/jz/accordin-m0/target/flexguard-suite-20260905/leveldb/out-static/db_bench')
EXPECTED_EXE = 'f958f932acbffe73bba697e3e19898141d78c6486f06dc9830896b171b81a96d'
DEFAULT_SEED = Path('/tmp/accordin-flexguard-suite-20260905/seed')
LOCK = '/tmp/mutexbench-sweep-multi-lock.lock'

BACKENDS = {'mcs_accordin': 'mcsaccordin', 'mcs_tas_accordin': 'mcstasaccordin'}
BENCHMARKS = ['fillrandom', 'readrandom']
THREADS = 192
TIME_MS = 30000

CONFIG = {
    'LC_ALL': 'C',
    'MCS_ACCORDIN_DIRECT_DISABLE_BPF': '0',
    'MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF': '0',
    'MCS_ACCORDIN_DIRECT_STATS_ONLY': '0',
    'MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY': '0',
    'ACCORDIN_DISABLE_ADMISSION': '0',
    'ACCORDIN_HOOK_STATS': '0',
    'ACCORDIN_CV_COUNTERS': '1',
    'OMP_PROC_BIND': 'false',
    'OMP_WAIT_POLICY': 'PASSIVE',
}
SCRUB = ('ACCORDIN_', 'MCS_ACCORDIN_', 'MCS_TAS_ACCORDIN_', 'SCX_', 'LD_', 'COND_VAR', 'COND_VAE')
ENV = {k: v for k, v in os.environ.items() if not k.startswith(SCRUB)} | CONFIG

ERROR_PATTERN = re.compile(
    r'put error|open error|Corruption:|Failed to (?:load|attach|register)|DEBUG DUMP|'
    r'Too many threads|Assertion.*failed|\[hook_stats\]')
TOTAL_PATTERN = re.compile(r'BENCH_TOTAL ops=(\d+) seconds=([\d.]+) ops_per_second=([\d.]+)')
COUNTER_KEYS = ['parked', 'flushed', 'expired', 'drained', 'parked_now', 'flush_calls', 'flush_misses']
COUNTER_PATTERN = re.compile(r'\[accordin_cv\] ' + ' '.join(f'{k}=(\\d+)' for k in COUNTER_KEYS))
DMESG_PATTERNS = ('ERROR_STALL', 'failed to run for', 'runnable task stall')


def read(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return ''


def state():
    return read('/sys/kernel/sched_ext/state')


def cell_key(sample):
    """Identity of one measurement slot: benchmark, backend, arm and repeat."""
    return (sample['benchmark'], sample['backend'], sample['arm'], sample['repeat'])


def load_results(path):
    """Read samples already recorded in an output directory, for resuming a session."""
    if not Path(path).is_file():
        return []
    return [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]


def seq():
    return int(read('/sys/kernel/sched_ext/enable_seq') or 0)


def sha(path):
    with Path(path).open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()


def inventory(path):
    return {str(f.relative_to(path)): {'bytes': f.stat().st_size, 'sha256': sha(f)}
            for f in sorted(Path(path).rglob('*')) if f.is_file()}


def command_output(cmd, cwd=None):
    return subprocess.check_output(cmd, text=True, cwd=cwd).strip()


def online_cpus():
    spec = read('/sys/devices/system/cpu/online')
    cpus = []
    for part in spec.split(','):
        if '-' in part:
            lo, hi = part.split('-')
            cpus.extend(range(int(lo), int(hi) + 1))
        elif part:
            cpus.append(int(part))
    return cpus


def wait_disabled(limit=120.0):
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        if state() == 'disabled':
            return
        time.sleep(0.1)
    raise RuntimeError(f'sched_ext state is {state()!r}, expected disabled')


def dmesg_lines():
    for cmd in (['sudo', '-n', 'dmesg', '-T'], ['dmesg', '-T']):
        try:
            return subprocess.run(cmd, capture_output=True, text=True,
                                  timeout=30).stdout.splitlines()
        except (OSError, subprocess.SubprocessError):
            continue
    try:
        fd = os.open('/dev/kmsg', os.O_RDONLY | os.O_NONBLOCK)
    except OSError:
        return None
    lines = []
    try:
        while True:
            try:
                chunk = os.read(fd, 8192)
            except BlockingIOError:
                break
            except OSError:
                break
            if not chunk:
                break
            lines.append(chunk.decode('utf-8', 'replace').strip())
    finally:
        os.close(fd)
    return lines


def dmesg_new(before, after):
    if before is None or after is None:
        return []
    if after[:len(before)] == before:
        return after[len(before):]
    seen = set(before)
    return [line for line in after if line not in seen]


def fix_owner(paths):
    uid, gid = os.environ.get('SUDO_UID'), os.environ.get('SUDO_GID')
    if not uid or not gid or os.geteuid() != 0:
        return
    for path in paths:
        if Path(path).exists():
            subprocess.run(['chown', '-R', f'{uid}:{gid}', str(path)], check=False)


def parse_arm(spec):
    name, _, rest = spec.partition('=')
    if not name or not rest:
        raise argparse.ArgumentTypeError(f'expected NAME=COMMIT[:ENV=VAL,...], got {spec!r}')
    commit, _, env_spec = rest.partition(':')
    overrides = {}
    for item in filter(None, env_spec.split(',')):
        key, sep, value = item.partition('=')
        if not sep:
            raise argparse.ArgumentTypeError(f'expected ENV=VAL, got {item!r}')
        overrides[key] = value
    return {'name': name, 'commit': commit, 'env': overrides}


def prepare_source(arm, sources, out):
    """Check out and build the arm's commit, reusing a worktree per distinct commit."""
    resolved = command_output(['git', '-C', str(ROOT), 'rev-parse', arm['commit'] + '^{commit}'])
    arm['resolved_commit'] = resolved
    if resolved in sources:
        arm['source'] = sources[resolved]
        return
    source = WORK / f'{arm["name"]}-src'
    if source.exists():
        head = command_output(['git', '-C', str(source), 'rev-parse', 'HEAD'])
        if head != resolved:
            raise RuntimeError(f'{source} is at {head}, expected {resolved}')
    else:
        source.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(['git', '-C', str(ROOT), 'worktree', 'add', '--detach', str(source), resolved],
                       check=True)
    build_env = {k: v for k, v in os.environ.items() if not k.startswith(SCRUB)}
    with (out / f'build-{arm["name"]}.log').open('w') as log:
        for cmd in (['make', '-j', 'all'], ['make', 'litl']):
            print('BUILD ' + ' '.join(cmd) + f' ({arm["name"]})', flush=True)
            subprocess.run(cmd, cwd=source, env=build_env, stdout=log,
                           stderr=subprocess.STDOUT, check=True)
    common = Path(command_output(['git', '-C', str(ROOT), 'rev-parse', '--git-common-dir']))
    if not common.is_absolute():
        common = (ROOT / common).resolve()
    fix_owner([source, common / 'worktrees' / source.name])
    dirty = command_output(['git', '-C', str(source), 'status', '--porcelain'])
    if dirty:
        raise RuntimeError(f'{source} is not clean:\n{dirty}')
    sources[resolved] = source
    arm['source'] = source


def make_cell(arm, backend):
    source = arm['source']
    libdir = source / 'target/release'
    adapter = source / f'third_party/litl/lib/lib{BACKENDS[backend]}_original.so'
    direct = libdir / f'lib{backend}_direct.so'
    for path in (adapter, direct):
        if not path.is_file():
            raise RuntimeError(f'missing library {path}')
    return {
        'id': f'{arm["name"]}-{backend}',
        'arm': arm['name'],
        'backend': backend,
        'commit': arm['resolved_commit'],
        'library': adapter,
        'libdir': libdir,
        'env': arm['env'],
        'expected_maps': sorted(str(p.resolve()) for p in (adapter, direct)),
    }


def stop(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
    wait_disabled()


def run(out, db, mode, cell, rep, attempt, cpus, time_ms):
    wait_disabled()
    ident = f'{mode}-{THREADS}-{cell["id"]}-r{rep}-a{attempt}'
    cmd = [str(EXE), f'--threads={THREADS}', f'--time_ms={time_ms}', f'--benchmarks={mode}',
           f'--use_existing_db={int(mode == "readrandom")}', f'--db={db}']
    overrides = CONFIG | {'LD_PRELOAD': str(cell['library']),
                          'LD_LIBRARY_PATH': str(cell['libdir'])} | cell['env']
    limit = time_ms / 1000.0 + 90
    sample = {'id': ident, 'benchmark': mode, 'arm': cell['arm'], 'backend': cell['backend'],
              'commit': cell['commit'], 'repeat': rep, 'attempt': attempt, 'command': cmd,
              'environment': overrides,
              'threads': THREADS, 'cpus': cpus, 'requested_time_ms': time_ms,
              'seq_before': seq(), 'scx_enabled': False, 'maps': [], 'bpf_fds': [],
              'max_process_threads': 0, 'timeout': False, 'expected_maps': cell['expected_maps']}
    print('START ' + ident, flush=True)
    kmsg_before = dmesg_lines()
    start = time.monotonic()
    with (out / (ident + '.log')).open('w') as log:
        process = subprocess.Popen(cmd, env=ENV | overrides, stdout=log,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            while process.poll() is None and time.monotonic() - start < limit:
                sample['scx_enabled'] |= state() == 'enabled'
                match = re.search(r'^Threads:\s+(\d+)', read(f'/proc/{process.pid}/status'), re.M)
                if match:
                    sample['max_process_threads'] = max(sample['max_process_threads'], int(match[1]))
                if not sample['maps'] or not sample['bpf_fds']:
                    sample['maps'] = sorted(set(
                        str(Path(line.split()[-1]).resolve())
                        for line in read(f'/proc/{process.pid}/maps').splitlines()
                        if any(x in line for x in ('accordin_direct', 'accordin_original'))))
                    for fd in Path(f'/proc/{process.pid}/fdinfo').glob('*'):
                        info = read(fd)
                        if re.search(r'^(map_id|prog_id|link_id):', info, re.M):
                            sample['bpf_fds'].append(info)
                time.sleep(0.1 if sample['maps'] and sample['bpf_fds'] else 0.005)
            sample['timeout'] = process.poll() is None
        finally:
            stop(process)
    sample.update(returncode=process.returncode, wall_seconds=time.monotonic() - start,
                  seq_after=seq(), state_after=state())
    text = (out / (ident + '.log')).read_text()
    total = TOTAL_PATTERN.search(text)
    if total:
        sample.update(operations=int(total[1]), roi_seconds=float(total[2]),
                      ops_per_second=float(total[3]))
    counters = COUNTER_PATTERN.findall(text)
    if counters:
        sample['counters'] = {k: int(v) for k, v in zip(COUNTER_KEYS, counters[-1])}
    stalls = [line for line in dmesg_new(kmsg_before, dmesg_lines())
              if any(pattern in line for pattern in DMESG_PATTERNS)]
    sample['kernel_stalls'] = stalls
    seconds = time_ms / 1000.0
    reasons = []
    if sample['timeout']:
        reasons.append(f'timeout after {limit:.0f}s')
    if process.returncode != 0:
        reasons.append(f'returncode {process.returncode}')
    if not total:
        reasons.append('no BENCH_TOTAL line')
    else:
        if sample['operations'] <= 0:
            reasons.append('zero operations')
        if not seconds - 1 <= sample['roi_seconds'] < seconds + 30:
            reasons.append(f'measurement window {sample["roi_seconds"]:.1f}s')
    if sample['maps'] != sample['expected_maps']:
        reasons.append(f'maps mismatch {sample["maps"]}')
    if not sample['bpf_fds']:
        reasons.append('no BPF file descriptors')
    if sample['max_process_threads'] < THREADS + 1:
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
    if Path(db).exists():
        entries = list(Path(db).iterdir())
        sample['db_after'] = {'files': len(entries),
                              'bytes': sum(f.stat().st_size for f in entries if f.is_file())}
    with (out / 'results.jsonl').open('a') as f:
        f.write(json.dumps(sample) + '\n')
    reported = {k: sample[k] for k in
                ['id', 'valid', 'reason', 'operations', 'roi_seconds', 'ops_per_second',
                 'max_process_threads', 'counters', 'kernel_stalls'] if k in sample}
    print('DONE ' + json.dumps(reported), flush=True)
    return sample


def summarize(samples, arms, benchmarks, backends):
    """Aggregate valid samples per benchmark x backend x arm, counting invalid attempts."""
    baseline_arm = arms[0]['name']
    valid = [s for s in samples if s.get('valid')]
    rows = []
    for mode in benchmarks:
        for backend in backends:
            def belongs(s, name):
                return (s['benchmark'] == mode and s['backend'] == backend and s['arm'] == name)

            def values(name):
                return [s['ops_per_second'] for s in valid
                        if belongs(s, name) and s.get('ops_per_second')]
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
                    'mean_ops_per_second': mean, 'stdev': sd if vals else None,
                    'cv_percent': 100 * sd / mean if mean else None,
                    'relative_to_' + baseline_arm: mean / base_mean if mean and base_mean else None,
                    'runs_ops_per_second': vals,
                    'mean_counters': {k: statistics.mean([c[k] for c in counters])
                                      for k in COUNTER_KEYS} if counters else None,
                })
    return baseline_arm, rows


def write_markdown(path, baseline_arm, rows, samples):
    lines = ['# LevelDB db_bench comparison of fixed commits', '',
             f'Throughput in Kops/s over valid samples only, '
             f'relative column against `{baseline_arm}`.', '']
    for mode in dict.fromkeys(r['benchmark'] for r in rows):
        for backend in dict.fromkeys(r['backend'] for r in rows if r['benchmark'] == mode):
            cell = [r for r in rows if r['benchmark'] == mode and r['backend'] == backend]
            lines += [f'## {mode} / {backend}', '',
                      '| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% |'
                      ' relative | runs |',
                      '|---|---|---:|---:|---:|---:|---:|---:|---|']
            for r in cell:
                rel = r['relative_to_' + baseline_arm]
                runs = ' / '.join(f'{v / 1000:.3f}' for v in r['runs_ops_per_second']) or '—'
                mean = r['mean_ops_per_second']
                lines.append(
                    f'| {r["arm"]} | `{r["commit"][:7]}` | {r["n_valid"]} | {r["n_invalid"]} | '
                    + (f'{mean / 1000:.3f}' if mean else '—') + ' | '
                    + (f'{r["stdev"] / 1000:.3f}' if mean else '—') + ' | '
                    + (f'{r["cv_percent"]:.2f}' if mean else '—') + ' | '
                    + (f'{rel:.3f}x' if rel else '—') + f' | {runs} |')
            lines.append('')
            if any(r['mean_counters'] for r in cell):
                lines += ['| arm | ' + ' | '.join(COUNTER_KEYS) + ' |',
                          '|---|' + '---:|' * len(COUNTER_KEYS)]
                for r in cell:
                    if r['mean_counters']:
                        lines.append(f'| {r["arm"]} | ' + ' | '.join(
                            f'{r["mean_counters"][k]:.1f}' for k in COUNTER_KEYS) + ' |')
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
    parser.add_argument('--arm', action='append', type=parse_arm, required=True, metavar='NAME=COMMIT[:ENV=VAL,...]')
    parser.add_argument('--out', type=Path)
    parser.add_argument('--seed', type=Path, default=DEFAULT_SEED)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--benchmarks', default=','.join(BENCHMARKS))
    parser.add_argument('--backends', default=','.join(BACKENDS))
    parser.add_argument('--max-attempts', type=int, default=3,
                        help='attempts per cell before it is left without a valid sample')
    parser.add_argument('--keep-invalid', action='store_true',
                        help='keep the database of an invalid run instead of deleting it')
    parser.add_argument('--preflight', action='store_true',
                        help='build, check inputs, and take one 5 s fillrandom sample')
    args = parser.parse_args()

    arms = args.arm
    names = [a['name'] for a in arms]
    if len(set(names)) != len(names):
        raise RuntimeError('arm names must be unique')
    benchmarks = [b for b in BENCHMARKS if b in args.benchmarks.split(',')]
    backends = [b for b in BACKENDS if b in args.backends.split(',')]
    if not benchmarks or not backends:
        raise RuntimeError('no benchmark or backend selected')
    if args.max_attempts < 1:
        raise RuntimeError('--max-attempts must be at least 1')
    if args.preflight:
        arms, benchmarks, backends = arms[:1], ['fillrandom'], ['mcs_tas_accordin']

    stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
    out = (args.out or WORK / (('preflight-' if args.preflight else '') + stamp)).resolve()
    out.mkdir(parents=True, exist_ok=True)
    previous = load_results(out / 'results.jsonl')
    if previous:
        print(f'RESUME {len(previous)} samples from {out / "results.jsonl"}', flush=True)

    try:
        guard = os.open(LOCK, os.O_RDWR | os.O_CREAT, 0o666)
    except OSError:
        guard = os.open(LOCK, os.O_RDONLY)
    fcntl.flock(guard, fcntl.LOCK_EX)
    cpus = online_cpus()
    wait_disabled()
    os.sched_setaffinity(0, cpus)
    if not EXE.is_file():
        raise RuntimeError(f'missing db_bench at {EXE}')
    if sha(EXE) != EXPECTED_EXE:
        raise RuntimeError(f'db_bench sha256 mismatch: {sha(EXE)}')
    if not args.seed.is_dir():
        raise RuntimeError(f'missing seed at {args.seed}')

    sources = {}
    for arm in arms:
        prepare_source(arm, sources, out)
    cells = [make_cell(arm, backend) for arm in arms for backend in backends]

    files = {EXE, Path(__file__).resolve()}
    for source in sources.values():
        for pattern in ('src/*.c', 'src/*.h', 'src/bpf/*.c', 'src/bpf/*.h', 'include/*.h',
                        'third_party/litl/src/accordin*.c'):
            files.update(source.glob(pattern))
        files.add(source / 'Makefile')
    for cell in cells:
        files.update(Path(p) for p in cell['expected_maps'])
    hashes = {str(p): sha(p) for p in sorted(files) if p.is_file()}
    seed_manifest = inventory(args.seed)

    meta = {
        'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'mode': 'preflight' if args.preflight else 'comparison',
        'arms': [{'name': a['name'], 'requested': a['commit'], 'commit': a['resolved_commit'],
                  'source': str(a['source']), 'env': a['env']} for a in arms],
        'uname': list(os.uname()),
        'compiler': command_output(['clang', '--version']),
        'litl_compiler': command_output(['cc', '--version']),
        'hashes': hashes, 'seed': str(args.seed), 'seed_manifest_files': len(seed_manifest),
        'configuration': CONFIG,
        'cpu_governors': sorted(set(read(p) for p in
                                    Path('/sys/devices/system/cpu').glob('cpu[0-9]*/cpufreq/scaling_governor'))),
        'cpu0_khz': read('/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq'),
        'threads': THREADS, 'cpus': cpus, 'cpu_online_spec': read('/sys/devices/system/cpu/online'),
        'num_keys': 1000000, 'value_bytes': 100,
        'requested_time_ms': 5000 if args.preflight else TIME_MS,
        'db_filesystem': '/tmp tmpfs', 'repeats': 1 if args.preflight else args.repeats,
        'max_attempts': args.max_attempts, 'resumed_samples': len(previous),
        'benchmarks': benchmarks, 'backends': backends,
        'dmesg_available': dmesg_lines() is not None,
    }
    (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')

    tmp = Path(tempfile.mkdtemp(prefix='accordin-cv-custody-', dir='/tmp'))
    try:
        if 'readrandom' in benchmarks:
            audit = tmp / 'seed-audit'
            shutil.copytree(args.seed, audit)
            with (out / 'seed-audit.log').open('w') as f:
                result = subprocess.run([str(EXE), '--benchmarks=readseq', '--threads=1',
                                         '--use_existing_db=1', f'--db={audit}'],
                                        env=ENV, stdout=f, stderr=subprocess.STDOUT, timeout=120)
            if result.returncode != 0 or not re.search(
                    r'BENCH_TOTAL ops=1000000 ', (out / 'seed-audit.log').read_text()):
                raise RuntimeError('seed audit failed')
            shutil.rmtree(audit)
            print('Seed audit: 1000000 entries', flush=True)

        samples = list(previous)
        exhausted = []
        repeats = 1 if args.preflight else args.repeats
        time_ms = 5000 if args.preflight else TIME_MS
        for rep in range(1, repeats + 1):
            rotate = (rep - 1) % len(arms)
            order = [c for arm in arms[rotate:] + arms[:rotate] for c in cells
                     if c['arm'] == arm['name']]
            for mode in benchmarks:
                for cell in order:
                    key = (mode, cell['backend'], cell['arm'], rep)
                    done = [s for s in samples if cell_key(s) == key]
                    if any(s.get('valid') for s in done):
                        print(f'SKIP {mode}-{cell["id"]}-r{rep} (valid sample recorded)',
                              flush=True)
                        continue
                    for attempt in range(len(done) + 1, args.max_attempts + 1):
                        db = tmp / f'{mode}-{cell["id"]}-r{rep}-a{attempt}'
                        if mode == 'readrandom':
                            shutil.copytree(args.seed, db)
                            if inventory(db) != seed_manifest:
                                raise RuntimeError('seed copy differs from seed')
                        sample = run(out, db, mode, cell, rep, attempt, cpus, time_ms)
                        samples.append(sample)
                        if sample['valid'] or not args.keep_invalid:
                            shutil.rmtree(db, ignore_errors=True)
                        else:
                            print(f'RETAINED {db}', flush=True)
                        if sample['valid']:
                            break
                        print(f'INVALID {sample["id"]}: {sample["reason"]}', flush=True)
                    else:
                        exhausted.append({'benchmark': mode, 'backend': cell['backend'],
                                          'arm': cell['arm'], 'repeat': rep,
                                          'attempts': args.max_attempts})
                        print(f'EXHAUSTED {mode}-{cell["id"]}-r{rep} after '
                              f'{args.max_attempts} attempts', flush=True)

        if inventory(args.seed) != seed_manifest:
            raise RuntimeError('seed changed during the session')
        changed = [p for p, h in hashes.items() if sha(p) != h]
        if changed:
            raise RuntimeError(f'inputs changed during the session: {changed}')
        for source in sources.values():
            dirty = command_output(['git', '-C', str(source), 'status', '--porcelain'])
            if dirty:
                raise RuntimeError(f'{source} became dirty:\n{dirty}')

        baseline_arm, rows = summarize(samples, arms, benchmarks, backends)
        invalid = [{'id': s['id'], 'benchmark': s['benchmark'], 'backend': s['backend'],
                    'arm': s['arm'], 'repeat': s['repeat'], 'attempt': s.get('attempt'),
                    'reason': s.get('reason', 'unknown')}
                   for s in samples if not s.get('valid')]
        (out / 'summary.json').write_text(json.dumps(
            {'baseline_arm': baseline_arm, 'cells': rows, 'invalid_runs': invalid,
             'cells_without_valid_sample': exhausted}, indent=2) + '\n')
        write_markdown(out / 'summary.md', baseline_arm, rows, samples)
        meta.update(completed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                    valid_runs=sum(1 for s in samples if s.get('valid')),
                    invalid_runs=len(invalid), max_attempts=args.max_attempts,
                    cells_without_valid_sample=exhausted,
                    hashes_unchanged=True, seed_unchanged=True, state_after=state())
        (out / 'metadata.json').write_text(json.dumps(meta, indent=2) + '\n')
        shutil.rmtree(tmp, ignore_errors=True)
        fix_owner([out])
        print('SUMMARY ' + json.dumps(rows), flush=True)
        print('OUTPUT ' + str(out), flush=True)
        if exhausted:
            print('INCOMPLETE ' + json.dumps(exhausted), flush=True)
            return 1
        return 0
    except BaseException:
        fix_owner([out])
        print(f'Remaining owned DBs retained in {tmp}', flush=True)
        raise


if __name__ == '__main__':
    raise SystemExit(main())
