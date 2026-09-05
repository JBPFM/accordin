#!/usr/bin/env python3
"""Alternate preserved Rust and C direct libraries with one mutexbench binary."""
import argparse
import csv
import datetime
import hashlib
import json
import os
from pathlib import Path
import shlex
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parent.parent
BACKENDS = ('mcs_accordin_direct', 'mcs_tas_accordin_direct')
SCX = Path('/sys/kernel/sched_ext')


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rust-lib-dir', type=Path, required=True)
    parser.add_argument('--c-lib-dir', type=Path, default=ROOT / 'target/release')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--threads', type=int, default=192)
    parser.add_argument('--cpus', default=','.join(map(str, sorted(os.sched_getaffinity(0)))))
    parser.add_argument('--critical-ns', type=int, default=300)
    parser.add_argument('--outside-ns', type=int, default=3000)
    parser.add_argument('--duration-ms', type=int, default=5000)
    parser.add_argument('--warmup-ms', type=int, default=1000)
    parser.add_argument('--repeats', type=int, default=5)
    args = parser.parse_args()
    if min(args.threads, args.duration_ms, args.repeats) < 1:
        parser.error('threads, duration and repeats must be positive')
    args.output.mkdir(parents=True, exist_ok=False)
    libraries = {name: directory.resolve() for name, directory in
                 (('rust', args.rust_lib_dir), ('c', args.c_lib_dir))}
    binary = ROOT / 'bench/mutexbench/mutex_bench'
    artifacts = [binary] + [directory / f'lib{backend}.so'
                           for directory in libraries.values() for backend in BACKENDS]
    metadata = {
        'date_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'config': {key: str(value) if isinstance(value, Path) else value
                   for key, value in vars(args).items()},
        'sha256': {str(path): sha256(path) for path in artifacts},
    }
    for command in (['uname', '-a'], ['lscpu'], ['clang', '--version'],
                    ['pkg-config', '--modversion', 'libbpf'], ['bpftool', 'version'],
                    ['git', 'rev-parse', 'HEAD'], ['git', '-C', 'bench/mutexbench', 'rev-parse', 'HEAD']):
        metadata[shlex.join(command)] = subprocess.check_output(command, cwd=ROOT, text=True).strip()
    (args.output / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(('ACCORDIN_', 'MCS_ACCORDIN_', 'MCS_TAS_ACCORDIN_', 'SCX_'))
           and key != 'LD_PRELOAD'}
    for backend in BACKENDS:
        env[backend.upper() + '_DISABLE_BPF'] = '0'
        env[backend.upper() + '_STATS_ONLY'] = '0'
    env['ACCORDIN_DISABLE_ADMISSION'] = '0'
    sudo = [] if os.geteuid() == 0 else ['sudo', '-n']
    rows = []
    with (args.output / 'results.csv').open('w') as output:
        fields = ['backend', 'implementation', 'repeat', 'throughput_ops_per_sec',
                  'elapsed_seconds', 'total_operations', 'avg_lock_hold_ns',
                  'avg_wait_ns_estimated', 'scx_enabled_samples', 'loadavg_before', 'log', 'command']
        writer = csv.DictWriter(output, fieldnames=fields)
        writer.writeheader()
        for repeat in range(1, args.repeats + 1):
            for backend in BACKENDS:
                for implementation in (('rust', 'c') if repeat % 2 else ('c', 'rust')):
                    if (SCX / 'state').read_text().strip() != 'disabled':
                        raise RuntimeError('A sched_ext scheduler is already running')
                    seq = int((SCX / 'enable_seq').read_text())
                    log = args.output / f'{backend}-{implementation}-{repeat}.log'
                    assignments = [f'{name.upper()}_LIB={libraries[implementation] / ("lib" + name + ".so")}'
                                   for name in BACKENDS]
                    assignments += [f'{key}={env[key]}' for key in env
                                    if key.startswith(('ACCORDIN_', 'MCS_ACCORDIN_', 'MCS_TAS_ACCORDIN_'))]
                    command = sudo + ['env', *assignments, 'taskset', '-c', args.cpus,
                        'timeout', '--kill-after=3s', f'{30 + (args.duration_ms + args.warmup_ms) / 1000}s',
                        str(binary), '--lock-kind', backend, '--threads', str(args.threads),
                        '--duration-ms', str(args.duration_ms), '--warmup-duration-ms', str(args.warmup_ms),
                        '--critical-ns', str(args.critical_ns), '--outside-ns', str(args.outside_ns),
                        '--workload', 'single', '--timing-sample-stride', '8', '--timeslice-extension', 'off']
                    loadavg = Path('/proc/loadavg').read_text().strip()
                    active_samples, last_enabled = 0, 0.0
                    with log.open('w') as handle:
                        process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=handle, stderr=subprocess.STDOUT)
                        while process.poll() is None:
                            if (SCX / 'state').read_text().strip() == 'enabled':
                                active_samples += 1
                                last_enabled = time.monotonic()
                            time.sleep(0.1)
                    finished = time.monotonic()
                    text = log.read_text()
                    if (process.returncode or not active_samples or finished - last_enabled > 0.5
                            or int((SCX / 'enable_seq').read_text()) != seq + 1
                            or (SCX / 'state').read_text().strip() != 'disabled'
                            or 'eBPF scheduler loaded successfully' not in text
                            or 'DEBUG DUMP' in text or 'EXIT:' in text):
                        raise RuntimeError(f'Benchmark or scheduler failed; inspect {log}')
                    metrics = dict(line.split(': ', 1) for line in text.splitlines() if ': ' in line)
                    row = {key: metrics[key] for key in fields[3:8]}
                    if float(row['throughput_ops_per_sec']) <= 0:
                        raise RuntimeError(f'No progress: {log}')
                    row.update(backend=backend, implementation=implementation, repeat=repeat,
                               scx_enabled_samples=active_samples, loadavg_before=loadavg,
                               log=log.name, command=shlex.join(command))
                    rows.append(row)
                    writer.writerow(row)
                    output.flush()
                    print(f'{backend} {implementation} {repeat}/{args.repeats}: '
                          f'{row["throughput_ops_per_sec"]} ops/s', flush=True)
        for backend in BACKENDS:
            medians = {}
            for implementation in libraries:
                values = [float(row['throughput_ops_per_sec']) for row in rows
                          if row['backend'] == backend and row['implementation'] == implementation]
                medians[implementation] = statistics.median(values)
                print(f'{backend} {implementation}: median={medians[implementation]:.2f}, '
                      f'range={min(values):.2f}..{max(values):.2f}')
            print(f'C / Rust change: {(medians["c"] / medians["rust"] - 1) * 100:+.2f}%')


if __name__ == '__main__':
    main()
