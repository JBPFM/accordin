#!/usr/bin/env python3
"""Sample the loaded scheduler's global state while a run is in flight.

bpftrace cannot read another program's BPF maps, so the admission slots and the
custody counters are read the only way available from outside: the scheduler's
`.bss` is an mmapped array map, `bpftool map dump` prints it, and the byte
offsets of `admission`, the custody gate and the counters come from the symbol
table of the object the runtime loaded.

Each line of the CSV is one sample:
  t_ms, owners_busy, owners_total, cv_parked_now, cv_parked,
  cv_released_dispatch, cv_released_timer, cv_released_syscall,
  cv_flush_calls, cv_flush_misses, cv_expired, cv_drained
The three release counters are disjoint, so their sum is the number of waits
that left custody through a release.
`owners_busy` counts online CPUs whose admission slot holds a ticket, so
`owners_busy == owners_total` is the "every slot occupied" condition.
"""
import argparse
import json
import struct
import subprocess
import sys
import time
from pathlib import Path

# CSV column to the `.bss` symbol it is read from. The custody gate is a word
# wrapped in a struct that owns a cache line, so its symbol begins at the word
# and still reads as one unsigned 64-bit value, like every counter here.
FIELDS = {
    'cv_parked_now': 'cv_parked_now_line',
    'cv_parked': 'cv_parked',
    'cv_released_dispatch': 'cv_released_dispatch',
    'cv_released_timer': 'cv_released_timer',
    'cv_released_syscall': 'cv_released_syscall',
    'cv_flush_calls': 'cv_flush_calls',
    'cv_flush_misses': 'cv_flush_misses',
    'cv_expired': 'cv_expired',
    'cv_drained': 'cv_drained',
}


def online_cpus():
    cpus = []
    for part in Path('/sys/devices/system/cpu/online').read_text().strip().split(','):
        if '-' in part:
            lo, hi = part.split('-')
            cpus.extend(range(int(lo), int(hi) + 1))
        elif part:
            cpus.append(int(part))
    return cpus


def symbol_offsets(obj):
    """Offsets and size of the .bss globals, from the object the runtime loads."""
    out = subprocess.check_output(['readelf', '-sW', str(obj)], text=True)
    sections = subprocess.check_output(['readelf', '-SW', str(obj)], text=True)
    bss = bss_size = None
    for line in sections.splitlines():
        if '] .bss' in line:
            fields = line.split(']')[1].split()
            bss = line.split(']')[0].split('[')[-1].strip()
            bss_size = int(fields[4], 16)
            break
    if bss is None:
        raise RuntimeError(f'no .bss section in {obj}')
    offsets = {'.bss_size': bss_size}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) < 8 or parts[6] != bss:
            continue
        offsets[parts[7]] = int(parts[1], 16)
    for name in ['admission'] + list(FIELDS.values()):
        if name not in offsets:
            raise RuntimeError(f'{name} not found in {obj} .bss')
    return offsets


def bss_map_id(name, wait_s=30.0):
    """The scheduler loads on the first contended lock, a moment after start."""
    deadline = time.monotonic() + wait_s
    while True:
        out = subprocess.check_output(['bpftool', '-j', 'map', 'show'], text=True)
        ids = [m['id'] for m in json.loads(out) if m.get('name') == name]
        if ids:
            return max(ids)
        if time.monotonic() >= deadline:
            raise RuntimeError(f'no BPF map named {name} after {wait_s}s')
        time.sleep(0.1)


def dump(map_id):
    result = subprocess.run(['bpftool', '-j', 'map', 'dump', 'id', str(map_id)],
                            capture_output=True, text=True)
    entries = json.loads(result.stdout or '{"error":"no output"}')
    if isinstance(entries, dict):
        raise LookupError(entries.get('error', 'unexpected reply'))
    values = entries[0].get('value') or entries[0].get('values')
    if isinstance(values, list) and values and isinstance(values[0], dict):
        values = values[0]['value']
    return bytes(int(b, 16) for b in values)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--object', required=True,
                        help='accordin.bpf.o of the build under measurement')
    parser.add_argument('--map-name', default='accordin.bss')
    parser.add_argument('--interval-ms', type=int, default=100)
    parser.add_argument('--duration-s', type=float, default=0.0,
                        help='0 runs until interrupted')
    parser.add_argument('--out', required=True)
    args = parser.parse_args()

    offsets = symbol_offsets(args.object)
    cpus = online_cpus()
    map_id = bss_map_id(args.map_name)
    owners = offsets['admission'] + 8  # struct admission_state: enabled, then owners[]

    # A loaded scheduler built from other sources lays its .bss out differently,
    # and reading it at these offsets would report plausible nonsense.
    probe = dump(map_id)
    if len(probe) != offsets['.bss_size']:
        raise RuntimeError(f'{args.map_name} is {len(probe)} bytes, but {args.object} '
                           f'has a {offsets[".bss_size"]} byte .bss; the loaded '
                           'scheduler is not this build')

    start = time.monotonic()
    busy_all = 0
    samples = 0
    with open(args.out, 'w') as f:
        f.write('t_ms,owners_busy,owners_total,' + ','.join(FIELDS) + '\n')
        try:
            while True:
                now = time.monotonic()
                if args.duration_s and now - start >= args.duration_s:
                    break
                try:
                    raw = dump(map_id)
                except (LookupError, subprocess.CalledProcessError, IndexError):
                    break  # the scheduler unloaded; the run is over
                busy = sum(1 for c in cpus
                           if struct.unpack_from('<Q', raw, owners + 8 * c)[0])
                values = [struct.unpack_from('<Q', raw, offsets[sym])[0]
                          for sym in FIELDS.values()]
                f.write(f'{(now - start) * 1000:.0f},{busy},{len(cpus)},'
                        + ','.join(str(v) for v in values) + '\n')
                f.flush()
                samples += 1
                busy_all += 1 if busy == len(cpus) else 0
                time.sleep(args.interval_ms / 1000.0)
        except KeyboardInterrupt:
            pass
    if samples:
        print(f'samples={samples} all_slots_busy_fraction={busy_all / samples:.4f}',
              file=sys.stderr)


if __name__ == '__main__':
    main()
