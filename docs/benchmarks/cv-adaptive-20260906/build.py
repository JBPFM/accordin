#!/usr/bin/env python3
"""Rebuild an archived candidate in a fresh directory, never over old results."""
import argparse
import fcntl
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile

ROOT = next(p for p in Path(__file__).resolve().parents if (p / 'src/direct.c').is_file())
BASE = '934bd1c80bcc15f063656d49afc4d7e5edf53fe8'
PATHS = ['Makefile', 'README.md', 'src', 'include', 'scripts', 'third_party/scx', 'third_party/litl']


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('name')
    parser.add_argument('--patch', type=Path)
    parser.add_argument('--work', type=Path, default=ROOT / 'target/cv-adaptive-20260906')
    args = parser.parse_args()
    work = args.work.resolve()
    dest = work / 'variants' / args.name
    if dest.exists():
        raise RuntimeError(f'Candidate exists: {dest}; choose a fresh --work directory')
    dest.mkdir(parents=True)
    archive = subprocess.check_output(['git', '-C', str(ROOT), 'archive', BASE, *PATHS])
    with tarfile.open(fileobj=io.BytesIO(archive)) as source:
        source.extractall(dest, filter='data')
    if args.patch:
        subprocess.run(['patch', '--batch', '--forward', '-p1', '-i', str(args.patch.resolve())],
                       cwd=dest, check=True)
    source_hashes = {str(p.relative_to(dest)): hashlib.sha256(p.read_bytes()).hexdigest()
                     for p in dest.rglob('*') if p.is_file()}
    (work / f'{args.name}-source.json').write_text(json.dumps(source_hashes, indent=2) + '\n')
    with open('/tmp/mutexbench-sweep-multi-lock.lock', 'r') as guard:
        fcntl.flock(guard, fcntl.LOCK_EX)
        with (work / f'build-{args.name}.log').open('w') as log:
            subprocess.run(['make', '-C', str(dest), '-j8', 'litl'], stdout=log,
                           stderr=subprocess.STDOUT, check=True)
    hashes = {str(p.relative_to(dest)): hashlib.sha256(p.read_bytes()).hexdigest()
              for p in dest.rglob('*') if p.is_file() and
              (p.suffix in ['.h', '.c', '.so'] or p.name == 'Makefile')}
    (work / f'{args.name}.json').write_text(json.dumps({'source': str(dest), 'hashes': hashes},
                                                    indent=2) + '\n')


if __name__ == '__main__':
    main()
