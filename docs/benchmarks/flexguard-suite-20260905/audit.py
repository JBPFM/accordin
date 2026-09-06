#!/usr/bin/env python3
"""Check coverage and runtime evidence without running another benchmark."""
import collections
import hashlib
import json
import math
import os
from pathlib import Path

D = Path(__file__).resolve().parent
R = D.parents[2] / 'target' / os.environ.get('FLEXGUARD_SUITE_NAME', 'flexguard-suite-20260905')
rows = [json.loads(line) for line in (R / 'results.jsonl').read_text().splitlines()]
samples = [r for r in rows if r['phase'] == 'performance']
workloads = ['scheduling', 'buckets', 'leveldb-readrandom', 'leveldb-fillrandom',
             'leveldb-fillseq', 'leveldb-readseq', 'leveldb-overwrite',
             'kyotocabinet', 'raytrace', 'dedup', 'volrend', 'streamcluster',
             'index', 'index-4k']
expected_maps = {
    'mcs_accordin': {'libmcsaccordin_original.so', 'libmcs_accordin_direct.so'},
    'mcs_tas_accordin': {'libmcstasaccordin_original.so', 'libmcs_tas_accordin_direct.so'},
    'flexguard': {'interpose.so'},
}
issues = []
ids = collections.Counter(r['id'] for r in samples)
issues.extend(f'duplicate: {ident}' for ident, n in ids.items() if n != 1)
for r in samples:
    if not (R / r['log']).is_file():
        issues.append(f"{r['id']}: missing log")
    if not r['valid']:
        continue
    checks = {
        'successful exit': r['returncode'] == 0 and not r.get('timeout'),
        'positive finite metric': math.isfinite(r['value']) and r['value'] > 0,
        'expected libraries': expected_maps[r['backend']] <= {Path(p).name for p in r['maps']},
        'BPF descriptors': bool(r['bpf_fds']),
        'scheduler released': r['state_after'] == 'disabled',
        'enable sequence': int(r['seq_after']) - int(r['seq_before']) == (r['backend'] != 'flexguard'),
        'scheduler observed': r['scx_enabled'] == (r['backend'] != 'flexguard'),
    }
    if r['benchmark'] == 'scheduling':
        checks['requested thread point'] = len(r['measurements']) == 1 and int(r['measurements'][0][0]) == r['threads']
    issues.extend(f"{r['id']}: {key}" for key, ok in checks.items() if not ok)

groups = []
for workload in workloads:
    for threads in [96, 192]:
        for backend in expected_maps:
            group = [r for r in samples if (r['benchmark'], r['threads'], r['backend']) == (workload, threads, backend)]
            valid = [r for r in group if r['valid']]
            failed = [r for r in group if not r['valid']]
            if sorted(r['repeat'] for r in valid) == [1, 2, 3] and not failed:
                status = 'three valid repetitions'
            elif not valid and len(failed) == 1:
                status = 'failed; later repetitions skipped'
            else:
                status = 'incomplete'
            groups.append(dict(benchmark=workload, threads=threads, backend=backend,
                               status=status, valid=len(valid), failed=len(failed)))

metadata = json.loads((D / 'metadata.json').read_text())
hashes = {}
for filename, expected in metadata['files'].items():
    path = Path(filename)
    actual = hashlib.sha256(path.read_bytes()).hexdigest() if path.exists() else None
    hashes[filename] = {'expected': expected, 'actual': actual, 'unchanged': expected == actual}
    if expected != actual:
        issues.append(f'changed binary: {filename}')

verification = [json.loads(line) for line in (R / 'verification.jsonl').read_text().splitlines()]
expected_verification = {f'performance-dedup-{t}-{b}-r1' for t in [96, 192] for b in expected_maps}
if {v['id'] for v in verification} != expected_verification or not all(v['dedup_round_trip'] and v['returncode'] == 0 for v in verification):
    issues.append('missing or unsuccessful Dedup round-trip verification')
artifacts = {a['id']: a['artifacts'] for a in (json.loads(line) for line in (R / 'artifacts.jsonl').read_text().splitlines())}
volrend_outputs = []
for r in samples:
    if r['valid'] and r['benchmark'] == 'volrend':
        output = artifacts.get(r['id'], [])
        frames = [a for a in output if a['name'].startswith('head_')]
        volrend_outputs.append(dict(id=r['id'], frames=len(frames), bytes=sum(a['bytes'] for a in frames)))
        if len(frames) != 3000:
            issues.append(f"{r['id']}: expected 3000 output frames, saw {len(frames)}")
audit = dict(attempts=len(samples), valid=sum(r['valid'] for r in samples),
             failures=[{k: r.get(k) for k in ['id', 'returncode', 'timeout', 'wall_seconds', 'log']} for r in samples if not r['valid']],
             duplicate_ids=[ident for ident, n in ids.items() if n != 1],
             completed_groups=sum(g['status'] != 'incomplete' for g in groups),
             expected_groups=len(groups), groups=groups, binary_hashes=hashes,
             dedup_verification=verification, volrend_outputs=volrend_outputs, issues=issues,
             scheduler_state_at_audit=Path('/sys/kernel/sched_ext/state').read_text().strip())
(D / 'audit.json').write_text(json.dumps(audit, indent=2) + '\n')
print(f"{len(samples)} attempts; {audit['valid']} valid; {audit['completed_groups']}/{len(groups)} groups complete; {len(issues)} evidence issues")
for issue in issues:
    print(issue)
