# Condvar-custody results

## Question

The condvar-custody series holds an untimed condvar waiter inside the
scheduler instead of letting it leave for the ordinary queue, and then changes
how the held waiter is handed back. Four changes on top of the custody
mechanism are under test:

- **tail insertion** — the release hands moved waits to the tail of the
  admission queue instead of the head (`ACCORDIN_CV_FLUSH_FLAGS=12`);
- **sharded lock-admission queue with a home-CPU idle kick** (`46180cd`) —
  waiters are filed into thirty-two per-CPU shards, a notified waiter is
  released into the shard of its own CPU and that CPU takes one idle kick;
  the older policy of sweeping every free admission slot after a release is
  kept behind `ACCORDIN_CV_FLUSH_FLAGS=26` as `shard-spread`;
- **shared notify slot** (`5c53e20`) — notifications are published into a BPF
  array mapped into the process instead of being read out of the waiter's own
  admission word, so a release no longer needs the notifier's address space;
- **dispatch-side release** (`20816bc`) — with notifications visible from any
  context, the release pass also runs at the top of `ops.dispatch` and from
  the custody timer.

A fifth change, the admission-walk cleanup (`ded83aa`) that sits on top of the
sharded queue, is measured for neutrality rather than for a gain.

A fifth workload joined once the tip was chosen: `mutex_bench` from
`bench/mutexbench` in direct-lock mode, a pure mutex loop with no condition
variables, run to price the admission-side changes on a workload where custody
never engages. A sixth asks whether that price is paid evenly: the two-lock
fairness workload of `experiments/run_experiment_six.py`, driven through the
same runner, splits the threads into two groups on two locks and reads the Jain
index of their normalized efficiencies alongside total throughput. A seventh
asks the same question one level down, between individual threads on a single
lock: the shape of `experiments/run_experiment_seven.py`, which counts each
thread's operations and reports the share taken by the busier half.

Two follow-up commits were screened after the series closed and are kept on
branch `cv_percpu_queue`: `1189963` gives every possible CPU id its own
admission queue in place of the thirty-two hash shards, with the walk rotating
over the queues that exist — 96 on this host, which has 96 possible CPU ids and
48 online — and `cec374b` adds a probe of the dispatching CPU's own queue ahead
of that rotation.

The reference point is `934bd1c`, the commit before the series. `9520d7d` is
the last change of the custody mechanism itself and is carried as an
intermediate arm.

### Acceptance rule

A change is accepted only against a baseline arm measured in the same session,
never against a number from another session. All four workloads are weighed
together: `fillrandom` and `readrandom` from LevelDB `db_bench`, the
streamcluster run and the 192-thread / 100-round barrier microbenchmark. A
change is accepted when it improves at least one workload by more than that
session's run-to-run spread and loses on none of the others by more than the
same spread; it is rejected when it loses against the arm it is meant to
replace, even if it still beats the baseline. Where two arms differ only by an
environment override, they are compared to each other, not to the baseline.
The `mutexbench` sessions came after the arms were chosen, so they enter as a
cost to weigh against those four workloads rather than as a gate of their own.

## Decision

Under that rule the series ends at `ded83aa`, the sharded admission queue plus
the admission-walk cleanup measured on top of it: neither the shared notify
slot nor the dispatch-side release passed validation.

- **`46180cd`, sharding with the home-CPU idle kick, is kept.** Branch
  `cv_admission` was reset to it and now carries the cleanup commit `ded83aa`
  on top, which the `final` session measures as neutral. `ded83aa` is the
  accepted tip.
- **`5c53e20`, the shared notify slot, was not kept on its own.** Against the
  `shard` arm of its own session it takes `fillrandom` from 225.443 to 229.930
  Kops/s and from 226.603 to 233.110 (1.020x and 1.029x) at a CV of 3.6–4.4 %,
  so the gain does not clear that session's spread; streamcluster is neutral
  (1.004x and 0.895x at CV 6.3 %); and `readrandom` falls from 1309.906 to
  1118.321 and from 1058.957 to 991.639 (0.854x and 0.936x). The attribution
  sessions place that `readrandom` loss in the shape of the code — map-value
  width together with an out-of-line lookup — rather than in work the slot does
  at run time, on a workload that parks about 200 waits in 30 s. It is still a
  measured loss set against no gain outside the spread.
- **`20816bc`, the dispatch-side release, is rejected.** It loses `fillrandom`
  against the `shard` arm of its own session, gains nothing elsewhere beyond
  the spread, and its `scan-off` arm shows that the syscall-only release path
  it builds on was degraded by the change.
- **The pure-mutex cost is carried.** `mutex_bench` prices the admission-side
  changes at up to 7 % below `934bd1c`, about half of that already present at
  `9520d7d`, and the two-lock fairness workload shows that cost falling on one
  group rather than on both, with no arm unfair on any case. Between individual
  threads the picture is different: sharding widens the per-thread spread at
  192 threads, most of it on the least-served thread. A single-repeat screen of
  two follow-up commits on `cv_percpu_queue` shows one queue per CPU id
  reproducing that spread, so it is not the shard geometry; the spread stays an
  open cost of sharding, and neither follow-up commit joins `cv_admission`,
  whose tip remains `ded83aa`. The own-queue probe of `cec374b` is left on that
  branch as a throughput-against-fairness trade for the user to decide. The sharding tip
  is kept regardless: on the same commits `fillrandom` runs at 3.77x and 3.73x
  of the `custody` arm and streamcluster at 0.517x and 0.514x of the baseline's
  seconds, so the gains on the workloads that park dominate the microbenchmark
  cost.
- Both commits, and a tidy-up `da90297` of the release path and its tests, are
  kept on branch `cv_slot_release` for the record; the `readrandom`
  attribution variants are on branch `readrandom-attrib`. Neither branch is
  merged.

## Environment

- 48 online CPUs (`0-47`), 192 worker threads in every run.
- `cpufreq` governor `performance`; `cpuidle` governor `menu`, driver
  `intel_idle`, C6 **enabled** (`disable` reads `0` on the CPUs sampled).
- LevelDB databases live on `/tmp` tmpfs, so no figure here is disk
  throughput.
- The host was rebooted early on 2026-09-07 UTC. Every session in the tables
  below except the last two ran after that reboot, between 14:45 UTC on
  2026-09-07 and 02:53 UTC on 2026-09-08, with the host up for 12 h or more.
- The machine drifts within a single day. The `fillrandom` / `mcs_accordin`
  baseline anchor alone reads 51.287, 47.100 and 52.551 Kops/s in three
  sessions a few hours apart, and 31.152 Kops/s on the day before the reboot.
  **Cross-session comparisons are therefore invalid.** Every table below is one
  session, and every ratio is against an arm measured in that same session.

## Sessions

Counts are written `valid/invalid`. `rel` is the ratio against the session's
first arm — throughput ratio for `fillrandom` and `readrandom` (higher is
better), mean-seconds ratio for `stream` and `barrier` (below 1.000 is faster).

### `h2-tail/leveldb` — tail insertion, LevelDB

| arm | commit | env | backend | fill n | fill Kops/s | CV% | rel | read n | read Kops/s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|---|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 3/0 | 51.287 | 2.52 | 1.000x | 3/0 | 1335.087 | 5.55 | 1.000x |
| custody | `9520d7d` | — | mcs_accordin | 3/0 | 61.019 | 1.16 | 1.190x | 3/0 | 1300.105 | 1.79 | 0.974x |
| custody-tail | `9520d7d` | `FLUSH_FLAGS=12` | mcs_accordin | 3/0 | 58.343 | 0.39 | 1.138x | 3/0 | 1299.029 | 1.76 | 0.973x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 3/0 | 57.618 | 7.38 | 1.000x | 3/0 | 1110.071 | 1.23 | 1.000x |
| custody | `9520d7d` | — | mcs_tas_accordin | 3/0 | 61.725 | 1.28 | 1.071x | 3/0 | 1097.344 | 4.45 | 0.989x |
| custody-tail | `9520d7d` | `FLUSH_FLAGS=12` | mcs_tas_accordin | 3/0 | 59.337 | 0.62 | 1.030x | 3/0 | 1015.369 | 6.90 | 0.915x |

Custody counters, `fillrandom`: roughly one park per operation (0.980–0.994
parks/op on both arms), expiries 0.024–0.026 % of parks. Tail insertion
lowers the flush rate — 12.0 k / 12.4 k flush calls against 19.3 k for head
insertion — and the miss rate with it, 15.5 % / 17.5 % against 27.8 % / 27.6 %.
`readrandom` parks about 380 waits in 30 s and issues two flush calls, so its
numbers say nothing about the custody path.

### `h2-tail/stream` — tail insertion, streamcluster and barrier

| arm | commit | env | backend | stream n | stream s | CV% | rel | barrier n | barrier s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|---|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 3/0 | 183.305 | 3.69 | 1.000x | 3/0 | 1.028 | 73.75 | 1.000x |
| custody | `9520d7d` | — | mcs_accordin | 3/0 | 143.900 | 1.56 | 0.785x | 3/0 | 0.647 | 7.52 | 0.630x |
| custody-tail | `9520d7d` | `FLUSH_FLAGS=12` | mcs_accordin | 3/0 | 144.202 | 0.39 | 0.787x | 3/0 | 0.674 | 13.21 | 0.656x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 3/0 | 176.769 | 4.77 | 1.000x | 3/0 | 0.794 | 48.00 | 1.000x |
| custody | `9520d7d` | — | mcs_tas_accordin | 3/0 | 142.896 | 0.65 | 0.808x | 3/0 | 0.696 | 10.56 | 0.876x |
| custody-tail | `9520d7d` | `FLUSH_FLAGS=12` | mcs_tas_accordin | 3/0 | 141.347 | 1.41 | 0.800x | 3/0 | 0.674 | 9.97 | 0.848x |

Counters: both arms park about 8.38 M waits over the streamcluster run
(≈ 58 k parks/s), expire 0.10–0.11 % of them, and miss under 0.6 % of flush
claims. The barrier parks ≈ 37.95 k waits, close to the 192 × 100 × 2 the
microbenchmark implies.

### `h1-shard/leveldb` — sharded admission queue, LevelDB

| arm | commit | env | backend | fill n | fill Kops/s | CV% | rel | read n | read Kops/s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|---|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 3/0 | 47.100 | 16.56 | 1.000x | 3/0 | 1380.929 | 4.28 | 1.000x |
| custody | `9520d7d` | — | mcs_accordin | 3/0 | 59.611 | 0.75 | 1.266x | 3/0 | 1290.529 | 1.26 | 0.935x |
| shard | `46180cd` | — | mcs_accordin | 3/0 | 224.738 | 0.69 | 4.772x | 3/0 | 1296.308 | 2.14 | 0.939x |
| shard-spread | `46180cd` | `FLUSH_FLAGS=26` | mcs_accordin | 3/0 | 183.815 | 0.93 | 3.903x | 3/0 | 1247.036 | 5.78 | 0.903x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 3/2 | 59.851 | 8.50 | 1.000x | 3/0 | 1102.232 | 0.44 | 1.000x |
| custody | `9520d7d` | — | mcs_tas_accordin | 3/0 | 60.760 | 0.93 | 1.015x | 3/0 | 1107.366 | 4.47 | 1.005x |
| shard | `46180cd` | — | mcs_tas_accordin | 3/0 | 226.669 | 0.48 | 3.787x | 3/0 | 1083.671 | 2.99 | 0.983x |
| shard-spread | `46180cd` | `FLUSH_FLAGS=26` | mcs_tas_accordin | 3/0 | 182.215 | 2.67 | 3.044x | 3/0 | 1082.227 | 0.93 | 0.982x |

The two invalid `fillrandom` attempts are both `baseline` on
`mcs_tas_accordin`: `r1-a1` timed out after 120 s with no `BENCH_TOTAL` line,
`r2-a1` timed out after 120 s.

Custody counters, `fillrandom`: the sharded arm parks 6.71 M / 6.75 M waits
against 1.76 M / 1.78 M for `custody`, still ≈ 0.99 parks/op, so the extra
parks are extra completed operations rather than extra parking per operation.
Its flush miss rate is the highest of the LevelDB arms, 49.1 % / 49.5 % of
144 k / 146 k calls. `shard-spread` parks fewer waits (5.50 M / 5.38 M), calls
the flush half as often with a lower miss rate (22.7 % / 24.3 % of 74 k), and
expires an order of magnitude more of what it parks, 0.254 % / 0.278 % against
0.033 % / 0.026 %.

### `h1-shard/stream` — sharded admission queue, streamcluster and barrier

| arm | commit | env | backend | stream n | stream s | CV% | rel | barrier n | barrier s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|---|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 3/0 | 181.380 | 4.60 | 1.000x | 3/0 | 0.585 | 70.76 | 1.000x |
| custody | `9520d7d` | — | mcs_accordin | 3/0 | 146.200 | 1.18 | 0.806x | 3/0 | 0.700 | 9.03 | 1.196x |
| shard | `46180cd` | — | mcs_accordin | 3/0 | 93.742 | 9.97 | 0.517x | 3/0 | 0.630 | 18.07 | 1.077x |
| shard-spread | `46180cd` | `FLUSH_FLAGS=26` | mcs_accordin | 3/0 | 90.473 | 5.56 | 0.499x | 3/0 | 0.307 | 15.51 | 0.525x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 3/0 | 180.420 | 2.89 | 1.000x | 3/0 | 0.355 | 13.42 | 1.000x |
| custody | `9520d7d` | — | mcs_tas_accordin | 3/0 | 147.211 | 0.87 | 0.816x | 3/0 | 0.691 | 11.48 | 1.948x |
| shard | `46180cd` | — | mcs_tas_accordin | 3/0 | 92.652 | 8.56 | 0.514x | 3/0 | 0.296 | 95.13 | 0.834x |
| shard-spread | `46180cd` | `FLUSH_FLAGS=26` | mcs_tas_accordin | 3/0 | 91.170 | 9.08 | 0.505x | 3/0 | 0.699 | 101.35 | 1.970x |

The barrier column of this session carries CV up to 101 % and its arms are not
separable; the streamcluster column is.

Counters: the sharded arms park at ≈ 90 k/s against ≈ 57 k/s for `custody`,
because the run is shorter for the same 8.4 M parks. Expiries rise from
0.12 % to 0.61 % of parks. `shard-spread` pays for its extra sweep in flush
traffic: 78.5 k / 78.1 k calls at a 38.8 % / 38.6 % miss rate, against 44.4 k
calls at 0.3 % for `shard`.

### `m1-slot/leveldb` — shared notify slot, LevelDB

| arm | commit | env | backend | fill n | fill Kops/s | CV% | rel | read n | read Kops/s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|---|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 3/0 | 52.551 | 9.94 | 1.000x | 3/0 | 1355.355 | 6.06 | 1.000x |
| shard | `46180cd` | — | mcs_accordin | 3/0 | 225.443 | 0.98 | 4.290x | 3/0 | 1309.906 | 4.64 | 0.966x |
| slot | `5c53e20` | — | mcs_accordin | 3/0 | 229.930 | 4.42 | 4.375x | 3/0 | 1118.321 | 4.24 | 0.825x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 3/2 | 56.496 | 3.54 | 1.000x | 3/0 | 1078.758 | 3.34 | 1.000x |
| shard | `46180cd` | — | mcs_tas_accordin | 3/0 | 226.603 | 0.92 | 4.011x | 3/0 | 1058.957 | 9.86 | 0.982x |
| slot | `5c53e20` | — | mcs_tas_accordin | 3/0 | 233.110 | 3.59 | 4.126x | 3/0 | 991.639 | 2.58 | 0.919x |

Both invalid attempts are `baseline` on `mcs_tas_accordin` `fillrandom`
(`r3-a1`, `r3-a2`), each a 120 s timeout.

Counters, `fillrandom`: the slot arm parks slightly more (6.90 M / 6.94 M
against 6.73 M / 6.76 M) and calls the flush considerably more often, 184 k /
179 k against 143 k / 145 k, at a higher miss rate, 58.4 % / 57.2 % against
48.5 % / 49.0 %. Its expiry share also rises, 0.040 % / 0.050 % against
0.025 % / 0.026 %.

### `m1-slot/stream` — shared notify slot, streamcluster and barrier

| arm | commit | env | backend | stream n | stream s | CV% | rel | barrier n | barrier s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|---|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 3/0 | 185.287 | 2.70 | 1.000x | 3/0 | 0.853 | 65.88 | 1.000x |
| shard | `46180cd` | — | mcs_accordin | 3/0 | 93.297 | 3.44 | 0.504x | 3/0 | 0.123 | 8.92 | 0.144x |
| slot | `5c53e20` | — | mcs_accordin | 3/0 | 93.668 | 6.27 | 0.506x | 3/0 | 0.670 | 68.40 | 0.785x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 3/0 | 176.083 | 8.31 | 1.000x | 3/0 | 1.210 | 26.68 | 1.000x |
| shard | `46180cd` | — | mcs_tas_accordin | 3/0 | 98.672 | 8.71 | 0.560x | 3/0 | 0.284 | 49.14 | 0.234x |
| slot | `5c53e20` | — | mcs_tas_accordin | 3/0 | 88.347 | 6.26 | 0.502x | 3/0 | 0.371 | 61.88 | 0.307x |

The barrier arms of this session carry CV between 8.9 % and 68.4 % and the
`slot` runs are bimodal (0.879 / 0.145 / 0.987 s on `mcs_accordin`), so the
barrier ranking here is not usable. Streamcluster counters are close between
the two arms: 8.43 M parks, ≈ 44.4 k flush calls, miss rate 0.2–0.3 %.

### `m1-readrandom/leveldb` — `readrandom` only, with the custody switch

| arm | commit | env | backend | read n | read Kops/s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 3/0 | 1366.284 | 5.41 | 1.000x |
| shard | `46180cd` | — | mcs_accordin | 3/0 | 1303.975 | 0.97 | 0.954x |
| shard-off | `46180cd` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1306.044 | 0.96 | 0.956x |
| slot | `5c53e20` | — | mcs_accordin | 3/0 | 1124.739 | 1.33 | 0.823x |
| slot-off | `5c53e20` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1163.499 | 1.21 | 0.852x |
| slot-one | `5c53e20` | `CV_SLOTS=1` | mcs_accordin | 3/0 | 1164.321 | 2.12 | 0.852x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 3/0 | 1113.335 | 4.68 | 1.000x |
| shard | `46180cd` | — | mcs_tas_accordin | 3/0 | 1098.369 | 4.98 | 0.987x |
| shard-off | `46180cd` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1095.725 | 2.06 | 0.984x |
| slot | `5c53e20` | — | mcs_tas_accordin | 3/0 | 1001.458 | 2.98 | 0.900x |
| slot-off | `5c53e20` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1003.291 | 1.56 | 0.901x |
| slot-one | `5c53e20` | `CV_SLOTS=1` | mcs_tas_accordin | 3/0 | 1000.744 | 1.94 | 0.899x |

The `-off` arms and `slot-one` record zero parks, zero flush calls: with a
single slot the control block leaves nothing for a thread to claim, so
`slot-one` measures the same inert custody path as `slot-off` rather than a
one-slot custody path. Turning custody off does not recover the loss —
`slot-off` is 0.891x / 0.916x of `shard-off` on the two backends — so the loss
at `5c53e20` does not come from the custody mechanism doing work.

### `m2-scan/leveldb` — dispatch-side release, LevelDB

| arm | commit | env | backend | fill n | fill Kops/s | CV% | rel | read n | read Kops/s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|---|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 3/0 | 50.031 | 4.13 | 1.000x | 3/0 | 1329.002 | 4.51 | 1.000x |
| shard | `46180cd` | — | mcs_accordin | 3/0 | 221.859 | 2.30 | 4.434x | 3/0 | 1300.790 | 1.33 | 0.979x |
| scan | `20816bc` | — | mcs_accordin | 3/0 | 209.682 | 0.75 | 4.191x | 3/0 | 1296.121 | 2.76 | 0.975x |
| scan-off | `20816bc` | `CV_SCAN=0` | mcs_accordin | 3/0 | 9.335 | 3.71 | 0.187x | 3/0 | 1253.575 | 6.51 | 0.943x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 3/1 | 64.304 | 13.23 | 1.000x | 3/0 | 1109.992 | 2.51 | 1.000x |
| shard | `46180cd` | — | mcs_tas_accordin | 3/0 | 223.664 | 0.98 | 3.478x | 3/0 | 1106.180 | 1.24 | 0.997x |
| scan | `20816bc` | — | mcs_tas_accordin | 3/0 | 212.832 | 1.54 | 3.310x | 3/0 | 1033.013 | 8.46 | 0.931x |
| scan-off | `20816bc` | `CV_SCAN=0` | mcs_tas_accordin | 3/0 | 11.775 | 6.77 | 0.183x | 3/0 | 1049.435 | 0.94 | 0.945x |

The one invalid attempt is `baseline` on `mcs_tas_accordin` `fillrandom`
(`r3-a1`), a 120 s timeout.

Counters, `fillrandom`: `shard` issues 139 k / 141 k flush calls at a 47.8 % /
48.4 % miss rate. The dispatch pass raises that to 1.18 M / 1.12 M calls while
lowering the miss rate to 1.3 % / 1.4 %; in
`fillrandom-192-scan-mcs_accordin-r1-a1`, 1.12 M of those calls are contended,
and of the 6.28 M waits released, 5.18 M leave through the notifier's syscall,
1.08 M through `ops.dispatch` and 17.7 k through the custody timer. `scan-off`
parks only 282 k / 287 k waits against 6.63 M / 6.66 M for `shard`, and 51.8 % /
52.0 % of what it parks leaves by expiry rather than by a release:
`fillrandom-192-scan-off-mcs_accordin-r1-a1` reads parked 293520, flushed
142109, expired 151411, flush_calls 2544, flush_skipped 1146, with every
release attributed to the syscall path (`rel_syscall` 142109, `rel_dispatch`
and `rel_timer` zero).

`readrandom` parks 194–273 waits per run on every arm here, so its column
carries no custody work on any of them.

### `m2-scan/stream` — dispatch-side release, streamcluster and barrier

| arm | commit | env | backend | stream n | stream s | CV% | rel | barrier n | barrier s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|---|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 3/0 | 175.741 | 3.88 | 1.000x | 3/0 | 0.349 | 10.58 | 1.000x |
| shard | `46180cd` | — | mcs_accordin | 3/0 | 99.269 | 0.92 | 0.565x | 3/0 | 0.304 | 107.34 | 0.871x |
| scan | `20816bc` | — | mcs_accordin | 3/0 | 97.577 | 4.96 | 0.555x | 3/0 | 0.386 | 105.34 | 1.105x |
| scan-off | `20816bc` | `CV_SCAN=0` | mcs_accordin | 3/0 | 102.952 | 3.83 | 0.586x | 3/0 | 0.541 | 57.43 | 1.549x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 3/0 | 176.628 | 1.33 | 1.000x | 3/0 | 1.110 | 50.00 | 1.000x |
| shard | `46180cd` | — | mcs_tas_accordin | 3/0 | 101.499 | 4.84 | 0.575x | 3/0 | 0.389 | 27.70 | 0.351x |
| scan | `20816bc` | — | mcs_tas_accordin | 3/0 | 104.941 | 4.11 | 0.594x | 3/0 | 0.226 | 97.92 | 0.203x |
| scan-off | `20816bc` | `CV_SCAN=0` | mcs_tas_accordin | 3/0 | 96.694 | 7.96 | 0.547x | 3/0 | 0.476 | 105.31 | 0.428x |

The barrier column of this session carries CV between 27.7 % and 107.3 % on the
three non-baseline arms and separates nothing.

Counters: all three arms park the same 8.42–8.43 M waits and expire 0.6 % of
them. The dispatch pass raises streamcluster flush calls from 44.4 k / 44.4 k
to 604 k / 571 k at a 2.8 % miss rate against 0.3 % / 0.3 %, without moving the
run time. `scan-off` is indistinguishable from `shard` here — 44.3 k calls,
0.1 % misses — so the `fillrandom` collapse is not a property of the
streamcluster path.

### `attrib-2/leveldb` — where the `readrandom` loss lives

Custody is off in every arm of the attribution sessions and the anchor is
`shard-off`, so only the shape of the code differs between arms.

| arm | commit | env | backend | read n | read Kops/s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|
| shard-off | `46180cd` | `CV_CUSTODY=0` | mcs_accordin | 4/0 | 1301.177 | 1.09 | 1.000x |
| slot-off | `5c53e20` | `CV_CUSTODY=0` | mcs_accordin | 4/0 | 1118.575 | 1.47 | 0.860x |
| head-off | `20816bc` | `CV_CUSTODY=0` | mcs_accordin | 4/0 | 1287.464 | 2.22 | 0.989x |
| shard-off | `46180cd` | `CV_CUSTODY=0` | mcs_tas_accordin | 4/0 | 1083.592 | 3.70 | 1.000x |
| slot-off | `5c53e20` | `CV_CUSTODY=0` | mcs_tas_accordin | 4/0 | 964.188 | 5.26 | 0.890x |
| head-off | `20816bc` | `CV_CUSTODY=0` | mcs_tas_accordin | 4/0 | 1091.978 | 0.18 | 1.008x |

The loss is present at `5c53e20` and gone again at `20816bc`, which does not
touch the registration record at all.

### `attrib-1/leveldb` — registration record and enqueue lookup

Variants on top of `20816bc`, each a small delta:

- `vA-narrow` (`52d0f1e`) — registration map value narrowed back to a bare
  address, slot identity moved into its own hash map read in the custody
  branch of enqueue;
- `vB-shape` (`c76b073`) — record kept wide, the enqueue lookup restored to
  its earlier shape (`user_state` performs its own lookup, the record is read
  only where the slot is needed);
- `vC-both` (`db98829`) — narrowed value with the separate map, plus the
  earlier enqueue lookup shape.

| arm | commit | env | backend | read n | read Kops/s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|
| shard-off | `46180cd` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1317.936 | 1.39 | 1.000x |
| head-off | `20816bc` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1310.443 | 2.07 | 0.994x |
| vA-narrow | `52d0f1e` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1144.930 | 2.38 | 0.869x |
| vB-shape | `c76b073` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1295.885 | 1.87 | 0.983x |
| vC-both | `db98829` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1116.214 | 0.99 | 0.847x |
| shard-off | `46180cd` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1096.255 | 4.89 | 1.000x |
| head-off | `20816bc` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1101.270 | 0.89 | 1.005x |
| vA-narrow | `52d0f1e` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1022.930 | 2.37 | 0.933x |
| vB-shape | `c76b073` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1120.722 | 1.70 | 1.022x |
| vC-both | `db98829` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1018.443 | 0.90 | 0.929x |

Narrowing the record and reading the slot out of a second map brings the loss
back on a commit that does not otherwise have it, in both enqueue lookup
shapes. Restoring only the enqueue lookup shape does not.

### `attrib-3/leveldb` — map allocation and record width

- `F-extramap` (`d0c86c6`) — `20816bc` plus a hash map of the same shape a
  separate slot table would have, declared and never read;
- `G-wide32` (`27c6f4b`) — `20816bc` with the registration record widened by
  one unused word.

| arm | commit | env | backend | read n | read Kops/s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|
| shard-off | `46180cd` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1302.800 | 3.15 | 1.000x |
| head-off | `20816bc` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1286.851 | 0.80 | 0.988x |
| F-extramap | `d0c86c6` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1318.626 | 1.67 | 1.012x |
| G-wide32 | `27c6f4b` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1302.183 | 2.83 | 1.000x |
| shard-off | `46180cd` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1089.876 | 1.35 | 1.000x |
| head-off | `20816bc` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1073.862 | 3.03 | 0.985x |
| F-extramap | `d0c86c6` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1106.043 | 1.53 | 1.015x |
| G-wide32 | `27c6f4b` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1112.275 | 0.83 | 1.021x |

Neither the map allocation on its own nor the wider record on its own moves
`readrandom`.

### `attrib-4/leveldb` — narrowing without a replacement table

`H-narrowonly` (`e7d6a27`) narrows the registration map value back to a bare
address and drops the slot identity entirely, with no second table to read it
from.

| arm | commit | env | backend | read n | read Kops/s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|
| shard-off | `46180cd` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1302.021 | 3.33 | 1.000x |
| head-off | `20816bc` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1327.891 | 1.98 | 1.020x |
| H-narrowonly | `e7d6a27` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 1302.602 | 1.75 | 1.000x |
| shard-off | `46180cd` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1095.063 | 2.29 | 1.000x |
| head-off | `20816bc` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1086.403 | 2.79 | 0.992x |
| H-narrowonly | `e7d6a27` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 1097.011 | 1.84 | 1.002x |

### `final/leveldb` — admission-walk cleanup, LevelDB

`tidy` is `ded83aa`, the tip of `cv_admission`: `46180cd` plus the
admission-walk cleanup and nothing else — an early stop when a slot is lost, an
unbiased cursor, and a shard-id helper.

| arm | commit | env | backend | fill n | fill Kops/s | CV% | rel | read n | read Kops/s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|---|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 3/0 | 47.618 | 12.17 | 1.000x | 3/0 | 1354.845 | 1.69 | 1.000x |
| shard | `46180cd` | — | mcs_accordin | 3/0 | 224.571 | 1.11 | 4.716x | 3/0 | 1292.503 | 1.40 | 0.954x |
| tidy | `ded83aa` | — | mcs_accordin | 3/0 | 225.541 | 1.04 | 4.736x | 3/0 | 1333.971 | 1.30 | 0.985x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 3/1 | 55.951 | 4.62 | 1.000x | 3/0 | 1103.569 | 1.92 | 1.000x |
| shard | `46180cd` | — | mcs_tas_accordin | 3/0 | 226.756 | 0.99 | 4.053x | 3/0 | 1116.324 | 0.53 | 1.012x |
| tidy | `ded83aa` | — | mcs_tas_accordin | 3/0 | 224.764 | 0.82 | 4.017x | 3/0 | 1089.773 | 1.61 | 0.987x |

The one invalid attempt is `baseline` on `mcs_tas_accordin` `fillrandom`
(`r3-a1`), a 120 s timeout with no `BENCH_TOTAL` line.

Counters, `fillrandom`: the two arms are indistinguishable — 6.71 M / 6.77 M
parks for `shard` against 6.74 M / 6.68 M for `tidy`, 144 k / 146 k flush calls
against 144 k / 142 k, and a miss rate of 49.1 % / 49.3 % against 48.7 % /
48.8 %.

### `final/stream` — admission-walk cleanup, streamcluster and barrier

This session has no baseline arm; `rel` is against `shard`.

| arm | commit | env | backend | stream n | stream s | CV% | rel | barrier n | barrier s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|---|---:|---:|---:|
| shard | `46180cd` | — | mcs_accordin | 3/0 | 94.952 | 6.40 | 1.000x | 3/0 | 0.421 | 123.82 | 1.000x |
| tidy | `ded83aa` | — | mcs_accordin | 3/0 | 94.704 | 8.28 | 0.997x | 3/0 | 0.228 | 83.35 | 0.541x |
| shard | `46180cd` | — | mcs_tas_accordin | 3/0 | 96.596 | 4.60 | 1.000x | 3/0 | 0.366 | 64.52 | 1.000x |
| tidy | `ded83aa` | — | mcs_tas_accordin | 3/0 | 99.321 | 3.83 | 1.028x | 3/0 | 0.316 | 53.94 | 0.863x |

The barrier column carries CV between 53.9 % and 123.8 % and separates nothing.
Counters match arm for arm: 8.43 M parks, 44.4 k flush calls, expiries at
0.60–0.66 % of parks and misses at 0.3 % of calls on both.

### `mutexbench-final` — pure mutex loop, retained arms

`mutex_bench` from `bench/mutexbench` in direct-lock mode: 192 threads, 5 s
measured after a 1 s warmup, five repeats, throughput in Mops/s where higher is
better. Each pair is a critical-section / outside-section nanosecond setting.
The workload uses no condition variables, so the custody counters read zero on
every arm and are omitted here. `hold` and `wait` are the run's mean
`avg_lock_hold_ns` and `avg_wait_ns_estimated`.

Pair 100 ns / 3000 ns:

| arm | commit | backend | n | Mops/s | CV% | rel | hold ns | wait ns |
|---|---|---|---|---:|---:|---:|---:|---:|
| baseline | `934bd1c` | mcs_accordin_direct | 5/0 | 2.697 | 2.20 | 1.000x | 147.4 | 70863.9 |
| shard | `46180cd` | mcs_accordin_direct | 5/0 | 2.616 | 2.08 | 0.970x | 146.5 | 73240.7 |
| tidy | `ded83aa` | mcs_accordin_direct | 5/0 | 2.548 | 3.27 | 0.945x | 146.6 | 75193.4 |
| baseline | `934bd1c` | mcs_tas_accordin_direct | 5/0 | 2.384 | 3.67 | 1.000x | 147.9 | 80261.2 |
| shard | `46180cd` | mcs_tas_accordin_direct | 5/0 | 2.291 | 2.55 | 0.961x | 147.5 | 83611.9 |
| tidy | `ded83aa` | mcs_tas_accordin_direct | 5/0 | 2.288 | 5.22 | 0.960x | 147.5 | 83866.2 |

Pair 300 ns / 3000 ns:

| arm | commit | backend | n | Mops/s | CV% | rel | hold ns | wait ns |
|---|---|---|---|---:|---:|---:|---:|---:|
| baseline | `934bd1c` | mcs_accordin_direct | 5/0 | 1.565 | 2.89 | 1.000x | 390.5 | 122291.3 |
| shard | `46180cd` | mcs_accordin_direct | 5/0 | 1.468 | 2.29 | 0.938x | 387.8 | 130137.1 |
| tidy | `ded83aa` | mcs_accordin_direct | 5/0 | 1.475 | 3.07 | 0.942x | 387.7 | 129562.1 |
| baseline | `934bd1c` | mcs_tas_accordin_direct | 5/0 | 1.351 | 7.42 | 1.000x | 391.6 | 141985.4 |
| shard | `46180cd` | mcs_tas_accordin_direct | 5/0 | 1.383 | 5.50 | 1.023x | 390.3 | 138555.3 |
| tidy | `ded83aa` | mcs_tas_accordin_direct | 5/0 | 1.328 | 5.66 | 0.983x | 390.1 | 144204.7 |

No attempt in this session was invalid.

### `mutexbench-attrib` — pure mutex loop, with the custody arm

The same points with `9520d7d` added, to place the cost before and after
sharding.

Pair 100 ns / 3000 ns:

| arm | commit | backend | n | Mops/s | CV% | rel | hold ns | wait ns |
|---|---|---|---|---:|---:|---:|---:|---:|
| baseline | `934bd1c` | mcs_accordin_direct | 5/0 | 2.674 | 1.31 | 1.000x | 147.2 | 71478.1 |
| custody | `9520d7d` | mcs_accordin_direct | 5/0 | 2.617 | 7.83 | 0.979x | 146.9 | 73426.1 |
| shard | `46180cd` | mcs_accordin_direct | 5/0 | 2.648 | 1.63 | 0.990x | 146.4 | 72268.3 |
| tidy | `ded83aa` | mcs_accordin_direct | 5/0 | 2.654 | 2.90 | 0.993x | 146.3 | 72069.6 |
| baseline | `934bd1c` | mcs_tas_accordin_direct | 5/0 | 2.336 | 5.94 | 1.000x | 147.6 | 82104.0 |
| custody | `9520d7d` | mcs_tas_accordin_direct | 5/0 | 2.231 | 3.76 | 0.955x | 147.8 | 85552.8 |
| shard | `46180cd` | mcs_tas_accordin_direct | 5/0 | 2.300 | 2.44 | 0.984x | 147.6 | 83138.5 |
| tidy | `ded83aa` | mcs_tas_accordin_direct | 5/0 | 2.333 | 5.90 | 0.999x | 147.9 | 82123.2 |

Pair 300 ns / 3000 ns:

| arm | commit | backend | n | Mops/s | CV% | rel | hold ns | wait ns |
|---|---|---|---|---:|---:|---:|---:|---:|
| baseline | `934bd1c` | mcs_accordin_direct | 5/0 | 1.548 | 4.72 | 1.000x | 391.2 | 123485.5 |
| custody | `9520d7d` | mcs_accordin_direct | 5/0 | 1.505 | 5.86 | 0.972x | 387.8 | 127228.9 |
| shard | `46180cd` | mcs_accordin_direct | 5/0 | 1.483 | 2.98 | 0.958x | 387.7 | 128924.3 |
| tidy | `ded83aa` | mcs_accordin_direct | 5/0 | 1.501 | 5.05 | 0.970x | 387.8 | 127388.6 |
| baseline | `934bd1c` | mcs_tas_accordin_direct | 5/0 | 1.418 | 3.01 | 1.000x | 390.9 | 134901.8 |
| custody | `9520d7d` | mcs_tas_accordin_direct | 5/0 | 1.382 | 4.52 | 0.975x | 390.1 | 138576.6 |
| shard | `46180cd` | mcs_tas_accordin_direct | 5/0 | 1.320 | 3.70 | 0.931x | 390.5 | 144741.3 |
| tidy | `ded83aa` | mcs_tas_accordin_direct | 5/0 | 1.335 | 2.25 | 0.941x | 391.1 | 143083.6 |

No attempt in this session was invalid.

### `fairness-final` — two-lock fairness

The two-lock workload of `experiments/run_experiment_six.py` run through the
same runner: 192 threads split into two groups of 96, each group on its own
lock, five repeats of 5 s after a 1 s warmup, all 120 runs valid. `Mops/s` is
the total of both groups and `rel` is against `baseline`. `Jain` is the index
over the two groups' normalized efficiencies, where 1.0000 means both groups
are held back equally against their own contention-free ideal; the parenthesis
gives the minimum and maximum over the five runs. `A` and `B` are the two
groups, and a slowdown is a group's own factor against that ideal. There are no
condition variables here either, so the custody counters read zero on every
arm.

Homogeneous — both groups at 300 ns critical / 3000 ns outside:

| arm | commit | backend | n | Mops/s | CV% | rel | Jain (min–max) | A Mops/s | B Mops/s | A slow | B slow |
|---|---|---|---|---:|---:|---:|---|---:|---:|---:|---:|
| baseline | `934bd1c` | mcs_accordin_direct | 5/0 | 3.224 | 4.20 | 1.000x | 1.0000 (1.0000–1.0000) | 1.613 | 1.611 | 18.07 | 18.08 |
| custody | `9520d7d` | mcs_accordin_direct | 5/0 | 2.978 | 4.23 | 0.924x | 1.0000 (0.9999–1.0000) | 1.482 | 1.496 | 19.66 | 19.48 |
| shard | `46180cd` | mcs_accordin_direct | 5/0 | 3.026 | 3.32 | 0.939x | 1.0000 (0.9999–1.0000) | 1.517 | 1.509 | 19.20 | 19.29 |
| tidy | `ded83aa` | mcs_accordin_direct | 5/0 | 3.005 | 6.63 | 0.932x | 1.0000 (0.9999–1.0000) | 1.506 | 1.499 | 19.37 | 19.49 |
| baseline | `934bd1c` | mcs_tas_accordin_direct | 5/0 | 2.922 | 4.36 | 1.000x | 1.0000 (1.0000–1.0000) | 1.465 | 1.457 | 19.89 | 19.99 |
| custody | `9520d7d` | mcs_tas_accordin_direct | 5/0 | 2.857 | 2.04 | 0.978x | 0.9999 (0.9998–1.0000) | 1.424 | 1.433 | 20.43 | 20.31 |
| shard | `46180cd` | mcs_tas_accordin_direct | 5/0 | 2.811 | 4.32 | 0.962x | 1.0000 (1.0000–1.0000) | 1.406 | 1.405 | 20.72 | 20.74 |
| tidy | `ded83aa` | mcs_tas_accordin_direct | 5/0 | 2.871 | 6.92 | 0.982x | 0.9999 (0.9997–1.0000) | 1.433 | 1.438 | 20.41 | 20.29 |

Heterogeneous, mild — group A at 3000 ns / 3000 ns against group B at
300 ns / 3000 ns:

| arm | commit | backend | n | Mops/s | CV% | rel | Jain (min–max) | A Mops/s | B Mops/s | A slow | B slow |
|---|---|---|---|---:|---:|---:|---|---:|---:|---:|---:|
| baseline | `934bd1c` | mcs_accordin_direct | 5/0 | 0.823 | 7.71 | 1.000x | 0.9412 (0.9222–0.9568) | 0.205 | 0.618 | 78.95 | 47.25 |
| custody | `9520d7d` | mcs_accordin_direct | 5/0 | 0.840 | 6.22 | 1.021x | 0.9480 (0.9355–0.9553) | 0.214 | 0.626 | 75.10 | 46.59 |
| shard | `46180cd` | mcs_accordin_direct | 5/0 | 0.781 | 1.70 | 0.949x | 0.9726 (0.9666–0.9817) | 0.220 | 0.561 | 72.73 | 51.87 |
| tidy | `ded83aa` | mcs_accordin_direct | 5/0 | 0.750 | 6.21 | 0.912x | 0.9641 (0.9540–0.9755) | 0.204 | 0.546 | 78.95 | 53.36 |
| baseline | `934bd1c` | mcs_tas_accordin_direct | 5/0 | 0.918 | 3.37 | 1.000x | 0.9469 (0.9457–0.9494) | 0.233 | 0.685 | 68.84 | 42.49 |
| custody | `9520d7d` | mcs_tas_accordin_direct | 5/0 | 0.922 | 2.99 | 1.004x | 0.9480 (0.9435–0.9514) | 0.234 | 0.687 | 68.26 | 42.38 |
| shard | `46180cd` | mcs_tas_accordin_direct | 5/0 | 0.826 | 6.00 | 0.900x | 0.9681 (0.9623–0.9743) | 0.228 | 0.598 | 70.37 | 48.82 |
| tidy | `ded83aa` | mcs_tas_accordin_direct | 5/0 | 0.818 | 4.37 | 0.891x | 0.9666 (0.9631–0.9701) | 0.224 | 0.594 | 71.46 | 49.09 |

Heterogeneous, extreme — group A at 3000 ns / 300 ns against group B at
100 ns / 3000 ns:

| arm | commit | backend | n | Mops/s | CV% | rel | Jain (min–max) | A Mops/s | B Mops/s | A slow | B slow |
|---|---|---|---|---:|---:|---:|---|---:|---:|---:|---:|
| baseline | `934bd1c` | mcs_accordin_direct | 5/0 | 0.574 | 7.99 | 1.000x | 0.8718 (0.8383–0.8905) | 0.170 | 0.404 | 173.51 | 76.91 |
| custody | `9520d7d` | mcs_accordin_direct | 5/0 | 0.556 | 9.85 | 0.969x | 0.8576 (0.8104–0.8768) | 0.159 | 0.397 | 188.34 | 78.29 |
| shard | `46180cd` | mcs_accordin_direct | 5/0 | 0.593 | 7.40 | 1.034x | 0.8850 (0.8545–0.9147) | 0.183 | 0.410 | 161.76 | 75.56 |
| tidy | `ded83aa` | mcs_accordin_direct | 5/0 | 0.589 | 5.17 | 1.026x | 0.8877 (0.8642–0.8993) | 0.182 | 0.407 | 160.64 | 76.20 |
| baseline | `934bd1c` | mcs_tas_accordin_direct | 5/0 | 0.699 | 3.89 | 1.000x | 0.9445 (0.9244–0.9540) | 0.255 | 0.445 | 114.19 | 69.83 |
| custody | `9520d7d` | mcs_tas_accordin_direct | 5/0 | 0.705 | 1.80 | 1.007x | 0.9427 (0.9343–0.9476) | 0.255 | 0.449 | 114.00 | 68.95 |
| shard | `46180cd` | mcs_tas_accordin_direct | 5/0 | 0.706 | 2.76 | 1.010x | 0.9422 (0.9283–0.9496) | 0.255 | 0.451 | 113.93 | 68.78 |
| tidy | `ded83aa` | mcs_tas_accordin_direct | 5/0 | 0.693 | 1.93 | 0.991x | 0.9479 (0.9432–0.9545) | 0.255 | 0.438 | 114.06 | 70.78 |

### `exp7-final` — per-thread fairness

The single-lock workload of `experiments/run_experiment_seven.py` run through
the same runner with per-thread accounting: critical sections of 100, 300, 1000
and 30000 ns against a 3000 ns outside section, thread counts 48, 96 and 192,
8 s measured after a 2 s warmup, three repeats, all 288 runs valid. `fairness`
is the share of all operations taken by the busier half of the threads, so
0.5000 is an even split and 1.0000 is one half of the threads doing everything;
the parenthesis gives the minimum and maximum over the three runs. `min ops`
and `max ops` are the mean least-served and best-served thread counts. No
condition variables are involved, so the custody counters read zero.

At 192 threads:

| critical | arm | commit | backend | n | Mops/s | rel | fairness (min–max) | min ops | max ops |
|---|---|---|---|---|---:|---:|---|---:|---:|
| 100 ns | baseline | `934bd1c` | mcs_accordin_direct | 3/0 | 2.662 | 1.000x | 0.5130 (0.5126–0.5134) | 101727 | 120254 |
| 100 ns | custody | `9520d7d` | mcs_accordin_direct | 3/0 | 2.603 | 0.978x | 0.5132 (0.5121–0.5137) | 98873 | 117668 |
| 100 ns | shard | `46180cd` | mcs_accordin_direct | 3/0 | 2.648 | 0.995x | 0.5183 (0.5177–0.5186) | 97869 | 125348 |
| 100 ns | tidy | `ded83aa` | mcs_accordin_direct | 3/0 | 2.628 | 0.987x | 0.5188 (0.5180–0.5194) | 97062 | 123663 |
| 100 ns | baseline | `934bd1c` | mcs_tas_accordin_direct | 3/0 | 2.396 | 1.000x | 0.5171 (0.5159–0.5196) | 88146 | 117795 |
| 100 ns | custody | `9520d7d` | mcs_tas_accordin_direct | 3/0 | 2.275 | 0.949x | 0.5201 (0.5185–0.5230) | 84085 | 112401 |
| 100 ns | shard | `46180cd` | mcs_tas_accordin_direct | 3/0 | 2.304 | 0.961x | 0.5251 (0.5222–0.5289) | 84253 | 115962 |
| 100 ns | tidy | `ded83aa` | mcs_tas_accordin_direct | 3/0 | 2.354 | 0.982x | 0.5229 (0.5210–0.5247) | 84998 | 118089 |
| 300 ns | baseline | `934bd1c` | mcs_accordin_direct | 3/0 | 1.544 | 1.000x | 0.5197 (0.5177–0.5213) | 56425 | 73469 |
| 300 ns | custody | `9520d7d` | mcs_accordin_direct | 3/0 | 1.487 | 0.963x | 0.5191 (0.5178–0.5203) | 54449 | 70750 |
| 300 ns | shard | `46180cd` | mcs_accordin_direct | 3/0 | 1.476 | 0.956x | 0.5301 (0.5267–0.5338) | 49880 | 75273 |
| 300 ns | tidy | `ded83aa` | mcs_accordin_direct | 3/0 | 1.517 | 0.982x | 0.5282 (0.5266–0.5312) | 51865 | 76441 |
| 300 ns | baseline | `934bd1c` | mcs_tas_accordin_direct | 3/0 | 1.357 | 1.000x | 0.5281 (0.5270–0.5291) | 47375 | 74280 |
| 300 ns | custody | `9520d7d` | mcs_tas_accordin_direct | 3/0 | 1.321 | 0.973x | 0.5309 (0.5289–0.5331) | 46993 | 72331 |
| 300 ns | shard | `46180cd` | mcs_tas_accordin_direct | 3/0 | 1.379 | 1.016x | 0.5348 (0.5311–0.5376) | 44503 | 74444 |
| 300 ns | tidy | `ded83aa` | mcs_tas_accordin_direct | 3/0 | 1.322 | 0.974x | 0.5388 (0.5347–0.5414) | 42759 | 74722 |
| 1000 ns | baseline | `934bd1c` | mcs_accordin_direct | 3/0 | 0.610 | 1.000x | 0.5285 (0.5263–0.5300) | 20958 | 30742 |
| 1000 ns | custody | `9520d7d` | mcs_accordin_direct | 3/0 | 0.593 | 0.973x | 0.5277 (0.5266–0.5291) | 20152 | 29709 |
| 1000 ns | shard | `46180cd` | mcs_accordin_direct | 3/0 | 0.607 | 0.995x | 0.5456 (0.5442–0.5480) | 18168 | 34764 |
| 1000 ns | tidy | `ded83aa` | mcs_accordin_direct | 3/0 | 0.604 | 0.991x | 0.5455 (0.5425–0.5487) | 17630 | 33509 |
| 1000 ns | baseline | `934bd1c` | mcs_tas_accordin_direct | 3/0 | 0.617 | 1.000x | 0.5432 (0.5379–0.5496) | 20110 | 36939 |
| 1000 ns | custody | `9520d7d` | mcs_tas_accordin_direct | 3/0 | 0.608 | 0.986x | 0.5466 (0.5421–0.5520) | 19894 | 36436 |
| 1000 ns | shard | `46180cd` | mcs_tas_accordin_direct | 3/0 | 0.611 | 0.990x | 0.5602 (0.5567–0.5626) | 17172 | 38292 |
| 1000 ns | tidy | `ded83aa` | mcs_tas_accordin_direct | 3/0 | 0.590 | 0.956x | 0.5591 (0.5550–0.5640) | 17320 | 36635 |
| 30000 ns | baseline | `934bd1c` | mcs_accordin_direct | 3/0 | 0.023 | 1.000x | 0.6509 (0.6446–0.6543) | 279 | 2212 |
| 30000 ns | custody | `9520d7d` | mcs_accordin_direct | 3/0 | 0.024 | 1.010x | 0.6477 (0.6447–0.6507) | 176 | 2217 |
| 30000 ns | shard | `46180cd` | mcs_accordin_direct | 3/0 | 0.022 | 0.947x | 0.6952 (0.6821–0.7033) | 93 | 2537 |
| 30000 ns | tidy | `ded83aa` | mcs_accordin_direct | 3/0 | 0.024 | 1.005x | 0.6970 (0.6865–0.7027) | 94 | 2790 |
| 30000 ns | baseline | `934bd1c` | mcs_tas_accordin_direct | 3/0 | 0.027 | 1.000x | 0.6466 (0.6434–0.6484) | 250 | 2393 |
| 30000 ns | custody | `9520d7d` | mcs_tas_accordin_direct | 3/0 | 0.027 | 1.003x | 0.6455 (0.6362–0.6542) | 300 | 2414 |
| 30000 ns | shard | `46180cd` | mcs_tas_accordin_direct | 3/0 | 0.026 | 0.996x | 0.6900 (0.6772–0.6996) | 155 | 2972 |
| 30000 ns | tidy | `ded83aa` | mcs_tas_accordin_direct | 3/0 | 0.026 | 0.989x | 0.7035 (0.6823–0.7142) | 122 | 2917 |

Mean fairness at the two lower thread counts, one column per arm:

| threads | critical | backend | baseline | custody | shard | tidy |
|---|---|---|---:|---:|---:|---:|
| 48 | 100 ns | mcs_accordin_direct | 0.5037 | 0.5021 | 0.5025 | 0.5025 |
| 48 | 100 ns | mcs_tas_accordin_direct | 0.5047 | 0.5034 | 0.5058 | 0.5043 |
| 48 | 300 ns | mcs_accordin_direct | 0.5034 | 0.5019 | 0.5022 | 0.5022 |
| 48 | 300 ns | mcs_tas_accordin_direct | 0.5045 | 0.5063 | 0.5069 | 0.5072 |
| 48 | 1000 ns | mcs_accordin_direct | 0.5023 | 0.5024 | 0.5022 | 0.5021 |
| 48 | 1000 ns | mcs_tas_accordin_direct | 0.5073 | 0.5143 | 0.5138 | 0.5155 |
| 48 | 30000 ns | mcs_accordin_direct | 0.5029 | 0.5027 | 0.5030 | 0.5033 |
| 48 | 30000 ns | mcs_tas_accordin_direct | 0.5332 | 0.5455 | 0.5432 | 0.5429 |
| 96 | 100 ns | mcs_accordin_direct | 0.5065 | 0.5068 | 0.5079 | 0.5078 |
| 96 | 100 ns | mcs_tas_accordin_direct | 0.5111 | 0.5134 | 0.5129 | 0.5131 |
| 96 | 300 ns | mcs_accordin_direct | 0.5091 | 0.5088 | 0.5115 | 0.5113 |
| 96 | 300 ns | mcs_tas_accordin_direct | 0.5157 | 0.5172 | 0.5210 | 0.5217 |
| 96 | 1000 ns | mcs_accordin_direct | 0.5142 | 0.5126 | 0.5201 | 0.5211 |
| 96 | 1000 ns | mcs_tas_accordin_direct | 0.5287 | 0.5294 | 0.5315 | 0.5375 |
| 96 | 30000 ns | mcs_accordin_direct | 0.5732 | 0.5718 | 0.6104 | 0.6096 |
| 96 | 30000 ns | mcs_tas_accordin_direct | 0.5950 | 0.5881 | 0.6232 | 0.6173 |

### `percpu` — per-CPU admission queues, single repeat

A screen of the two `cv_percpu_queue` commits against `934bd1c` and the
retained tip. **Every figure in these three tables comes from a single run per
cell**, so no spread is available and nothing here separates arms that differ
by a few percent. The three sessions ran back to back: per-thread fairness at
192 threads over the four critical sections, LevelDB `fillrandom` and
`readrandom`, and streamcluster without the barrier.

Per-thread fairness, 192 threads, one repeat:

| critical | arm | commit | backend | Mops/s | rel | fairness | min ops | max ops |
|---|---|---|---|---:|---:|---:|---:|---:|
| 100 ns | baseline | `934bd1c` | mcs_accordin_direct | 2.521 | 1.000x | 0.5131 | 97521 | 114574 |
| 100 ns | tidy | `ded83aa` | mcs_accordin_direct | 2.650 | 1.051x | 0.5171 | 97470 | 123799 |
| 100 ns | percpu | `1189963` | mcs_accordin_direct | 2.605 | 1.033x | 0.5174 | 95590 | 124272 |
| 100 ns | own | `cec374b` | mcs_accordin_direct | 2.654 | 1.053x | 0.6170 | 58437 | 229319 |
| 100 ns | baseline | `934bd1c` | mcs_tas_accordin_direct | 2.260 | 1.000x | 0.5202 | 82234 | 114685 |
| 100 ns | tidy | `ded83aa` | mcs_tas_accordin_direct | 2.307 | 1.021x | 0.5240 | 83118 | 115765 |
| 100 ns | percpu | `1189963` | mcs_tas_accordin_direct | 2.305 | 1.020x | 0.5234 | 77396 | 110744 |
| 100 ns | own | `cec374b` | mcs_tas_accordin_direct | 2.364 | 1.046x | 0.6172 | 54150 | 193404 |
| 300 ns | baseline | `934bd1c` | mcs_accordin_direct | 1.463 | 1.000x | 0.5201 | 54691 | 68807 |
| 300 ns | tidy | `ded83aa` | mcs_accordin_direct | 1.491 | 1.019x | 0.5268 | 48893 | 72330 |
| 300 ns | percpu | `1189963` | mcs_accordin_direct | 1.558 | 1.065x | 0.5250 | 55011 | 76106 |
| 300 ns | own | `cec374b` | mcs_accordin_direct | 1.470 | 1.005x | 0.6392 | 30169 | 128871 |
| 300 ns | baseline | `934bd1c` | mcs_tas_accordin_direct | 1.392 | 1.000x | 0.5294 | 49455 | 71669 |
| 300 ns | tidy | `ded83aa` | mcs_tas_accordin_direct | 1.344 | 0.965x | 0.5419 | 40624 | 75260 |
| 300 ns | percpu | `1189963` | mcs_tas_accordin_direct | 1.403 | 1.008x | 0.5315 | 48536 | 74724 |
| 300 ns | own | `cec374b` | mcs_tas_accordin_direct | 1.214 | 0.872x | 0.6125 | 29231 | 99540 |
| 1000 ns | baseline | `934bd1c` | mcs_accordin_direct | 0.616 | 1.000x | 0.5322 | 21352 | 30484 |
| 1000 ns | tidy | `ded83aa` | mcs_accordin_direct | 0.614 | 0.998x | 0.5473 | 18056 | 34729 |
| 1000 ns | percpu | `1189963` | mcs_accordin_direct | 0.630 | 1.023x | 0.5436 | 19162 | 36148 |
| 1000 ns | own | `cec374b` | mcs_accordin_direct | 0.558 | 0.906x | 0.5970 | 13320 | 50027 |
| 1000 ns | baseline | `934bd1c` | mcs_tas_accordin_direct | 0.598 | 1.000x | 0.5505 | 18631 | 36728 |
| 1000 ns | tidy | `ded83aa` | mcs_tas_accordin_direct | 0.570 | 0.952x | 0.5644 | 14688 | 40010 |
| 1000 ns | percpu | `1189963` | mcs_tas_accordin_direct | 0.621 | 1.038x | 0.5482 | 16097 | 36798 |
| 1000 ns | own | `cec374b` | mcs_tas_accordin_direct | 0.598 | 1.000x | 0.6195 | 12078 | 45913 |
| 30000 ns | baseline | `934bd1c` | mcs_accordin_direct | 0.023 | 1.000x | 0.6576 | 199 | 2202 |
| 30000 ns | tidy | `ded83aa` | mcs_accordin_direct | 0.022 | 0.950x | 0.7032 | 136 | 2510 |
| 30000 ns | percpu | `1189963` | mcs_accordin_direct | 0.023 | 0.968x | 0.7086 | 147 | 2418 |
| 30000 ns | own | `cec374b` | mcs_accordin_direct | 0.024 | 1.041x | 0.6886 | 163 | 3060 |
| 30000 ns | baseline | `934bd1c` | mcs_tas_accordin_direct | 0.027 | 1.000x | 0.6520 | 170 | 2599 |
| 30000 ns | tidy | `ded83aa` | mcs_tas_accordin_direct | 0.027 | 1.002x | 0.6793 | 128 | 2871 |
| 30000 ns | percpu | `1189963` | mcs_tas_accordin_direct | 0.026 | 0.982x | 0.6974 | 121 | 2596 |
| 30000 ns | own | `cec374b` | mcs_tas_accordin_direct | 0.026 | 0.987x | 0.6910 | 66 | 3033 |

LevelDB, one repeat (`fill` and `read` in Kops/s, `rel` against `baseline`):

| arm | commit | backend | fill Kops/s | rel | read Kops/s | rel |
|---|---|---|---:|---:|---:|---:|
| baseline | `934bd1c` | mcs_accordin | 59.009 | 1.000x | 1341.266 | 1.000x |
| tidy | `ded83aa` | mcs_accordin | 225.140 | 3.815x | 1303.925 | 0.972x |
| percpu | `1189963` | mcs_accordin | 223.748 | 3.792x | 1287.287 | 0.960x |
| own | `cec374b` | mcs_accordin | 255.149 | 4.324x | 1324.679 | 0.988x |
| baseline | `934bd1c` | mcs_tas_accordin | 55.854 | 1.000x | 1141.822 | 1.000x |
| tidy | `ded83aa` | mcs_tas_accordin | 221.413 | 3.964x | 1127.916 | 0.988x |
| percpu | `1189963` | mcs_tas_accordin | 225.600 | 4.039x | 1113.470 | 0.975x |
| own | `cec374b` | mcs_tas_accordin | 256.956 | 4.600x | 1075.664 | 0.942x |

Counters, `fillrandom`: `own` parks 7.61 M / 7.65 M waits against 6.73 M /
6.60 M for `tidy` and 6.68 M / 6.73 M for `percpu`, and calls the flush 180 k /
180 k times against 144 k / 140 k and 141 k / 145 k, at a miss rate near 53 %
against 49 % — more parks for more completed operations, on the same shape of
release traffic.

streamcluster, one repeat, seconds (lower is better, `rel` against `baseline`):

| arm | commit | backend | stream s | rel |
|---|---|---|---:|---:|
| baseline | `934bd1c` | mcs_accordin | 136.622 | 1.000x |
| tidy | `ded83aa` | mcs_accordin | 114.653 | 0.839x |
| percpu | `1189963` | mcs_accordin | 115.167 | 0.843x |
| own | `cec374b` | mcs_accordin | 93.261 | 0.683x |
| baseline | `934bd1c` | mcs_tas_accordin | 144.843 | 1.000x |
| tidy | `ded83aa` | mcs_tas_accordin | 111.716 | 0.771x |
| percpu | `1189963` | mcs_tas_accordin | 102.243 | 0.706x |
| own | `cec374b` | mcs_tas_accordin | 105.645 | 0.729x |

All arms park the same 8.43 M waits with 44.4 k flush calls; `own` expires
41.2 k of them on `mcs_accordin` against 62.0 k for `tidy`.

### `20260906T180338Z` — LevelDB before the reboot

Same binaries as the `baseline` and `custody` arms of `h2-tail`, plus a
custody-off arm.

| arm | commit | env | backend | fill n | fill Kops/s | CV% | rel | read n | read Kops/s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|---|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 3/0 | 31.152 | 1.02 | 1.000x | 3/1 | 1507.194 | 2.48 | 1.000x |
| custody | `9520d7d` | — | mcs_accordin | 3/0 | 39.909 | 48.44 | 1.281x | 3/0 | 1480.636 | 1.25 | 0.982x |
| custody-off | `9520d7d` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 34.841 | 6.40 | 1.118x | 3/0 | 1436.546 | 2.70 | 0.953x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 3/0 | 34.131 | 5.31 | 1.000x | 3/0 | 1290.913 | 1.49 | 1.000x |
| custody | `9520d7d` | — | mcs_tas_accordin | 3/0 | 39.647 | 47.56 | 1.162x | 3/0 | 1290.284 | 2.83 | 1.000x |
| custody-off | `9520d7d` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 31.904 | 1.53 | 0.935x | 3/0 | 1278.791 | 1.57 | 0.991x |

### `streamcluster-20260906T224228Z` — streamcluster before the reboot

| arm | commit | env | backend | stream n | stream s | CV% | rel | barrier n | barrier s | CV% | rel |
|---|---|---|---|---|---:|---:|---:|---|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 3/0 | 200.340 | 7.79 | 1.000x | 3/0 | 0.754 | 32.16 | 1.000x |
| custody | `9520d7d` | — | mcs_accordin | 3/0 | 280.740 | 2.46 | 1.401x | 3/0 | 1.327 | 3.60 | 1.760x |
| custody-off | `9520d7d` | `CV_CUSTODY=0` | mcs_accordin | 3/0 | 142.636 | 17.35 | 0.712x | 3/0 | 0.739 | 5.67 | 0.980x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 3/0 | 188.170 | 2.48 | 1.000x | 3/0 | 0.818 | 32.37 | 1.000x |
| custody | `9520d7d` | — | mcs_tas_accordin | 3/0 | 280.165 | 2.06 | 1.489x | 3/0 | 1.288 | 5.19 | 1.574x |
| custody-off | `9520d7d` | `CV_CUSTODY=0` | mcs_tas_accordin | 3/0 | 131.970 | 44.12 | 0.701x | 3/0 | 0.766 | 4.32 | 0.936x |

## Instruction budget

Verifier instruction counts reported by `make verify-insns` for the programs
the series touches. A dash marks a count that was not recorded.

| commit | arm | dispatch | enqueue | cv_flush |
|---|---|---:|---:|---:|
| `934bd1c` / `9520d7d` | baseline, custody | 186 | 405 | — |
| `46180cd` | shard | 342 | 415 | 3343 |
| `5c53e20` | slot | 342 | 508 | 3563 |
| `20816bc` | scan | 1563 | 508 | 4050 |
| `ded83aa` | tidy | 256 | 415 | 3343 |

Sharding costs the dispatch program 186 → 342 and adds the flush program.
The slot array leaves dispatch alone and adds 93 instructions to enqueue. The
dispatch-side release multiplies dispatch by 4.6, to 1563, which is the program
every dispatch decision pays for; the admission-walk cleanup `ded83aa` takes
dispatch back to 256 and returns enqueue and the flush to their `46180cd`
counts.

## Verdicts

### Tail insertion — rejected

Measured against `custody` in `h2-tail`, tail insertion costs `fillrandom`
4.4 % on `mcs_accordin` (58.343 against 61.019 Kops/s) and 3.9 % on
`mcs_tas_accordin` (59.337 against 61.725), with CV under 1.3 % on both arms,
so the loss is outside the run-to-run spread. Streamcluster is neutral
(144.202 against 143.900 s, and 141.347 against 142.896 s: ratios 1.002 and
0.989 against a CV of 0.4–1.6 %). The barrier column cannot separate them.
`readrandom` moves against tail insertion on `mcs_tas_accordin` (1015.369
against 1097.344, 0.925x) but at CV 6.9 %. It buys a genuinely cheaper
release — flush calls drop by about a third and misses by half — and that is
not enough to pay for the `fillrandom` loss. The default keeps head insertion
with reverse iteration.

### Sharded admission queue with home-CPU idle kick — accepted

In `h1-shard`, sharding is the largest move of the series. `fillrandom` goes
from 59.611 to 224.738 Kops/s on `mcs_accordin` and from 60.760 to 226.669 on
`mcs_tas_accordin` against the `custody` arm — 3.77x and 3.73x, or 4.77x and
3.79x against the session baseline — with CV under 1 % on the sharded arms.
Streamcluster goes from 146.200 to 93.742 s and from 147.211 to 92.652 s,
0.517x and 0.514x of the session baseline. `readrandom` is within noise:
0.939x and 0.983x of baseline, against a `custody` arm at 0.935x and 1.005x.
Barrier in this session is unusable (CV up to 95 %), but `m1-slot/stream`
later measures the same commit at 0.144x and 0.234x of its baseline.

### Spread kicks — rejected

`shard-spread` differs from `shard` only by the extra free-slot sweep after a
release. It costs `fillrandom` 18 % and 20 % (183.815 against 224.738;
182.215 against 226.669) at CV under 3 %, leaves streamcluster unchanged
(0.965x and 0.984x of `shard`, inside a 5.6–10 % CV), and leaves `readrandom`
unchanged (0.962x and 0.999x). Its counters show why the sweep is not free:
the flush miss rate rises to 38.8 % / 38.6 % on streamcluster against 0.3 %
for `shard`, and the expiry share on `fillrandom` rises about eightfold. The
home-CPU idle kick alone is kept; the sweep stays behind the flag.

### Shared notify slot — not kept

Against `shard` in `m1-slot`, the slot arm is neutral to slightly ahead on
`fillrandom` (1.020x and 1.029x, CV 3.6–4.4 %) and on streamcluster (1.004x
and 0.895x, CV 6.3 %). Neither move clears its own session's spread. The
barrier column of that session is bimodal and carries no verdict.

`readrandom` at `5c53e20` is a real loss in its own session: 0.854x and
0.936x of `shard` in `m1-slot`, reproduced at 0.863x and 0.912x in
`m1-readrandom`. It survives with custody switched off (`slot-off` at 0.891x
and 0.916x of `shard-off`), and `slot-one` — which parks nothing at all —
lands on the same number, so the mechanism is not spending time on `readrandom`.

The attribution sessions narrow it to the shape of the code rather than to
anything the slot does at run time:

- the loss is present at `5c53e20` (0.860x / 0.890x) and absent at `20816bc`
  (0.989x / 1.008x), and `20816bc` does not change the registration record;
- declaring an unused hash map of the slot table's shape is neutral
  (`F-extramap`, 1.012x / 1.015x);
- widening the registration record by one unused word is neutral
  (`G-wide32`, 1.000x / 1.021x);
- narrowing the record with no replacement table is neutral
  (`H-narrowonly`, 1.000x / 1.002x);
- narrowing the record **and** reading the slot identity from a separate map
  reintroduces the loss on the current head, in both enqueue lookup shapes
  (`vA-narrow` 0.869x / 0.933x, `vC-both` 0.847x / 0.929x), while restoring
  only the enqueue lookup shape does not (`vB-shape` 0.983x / 1.022x).

So a `readrandom` swing of 13–15 % follows a change of map-value width plus an
out-of-line lookup that is semantically inert on this workload — `readrandom`
parks about 200 waits in 30 s. The loss does not reproduce at the head of the
series, and no attribution arm identifies a cost the slot mechanism itself
pays. It is recorded here rather than explained. Since the commit shows no gain
outside the spread to set against it, the slot array is not carried on
`cv_admission`; it stays on `cv_slot_release`.

### Dispatch-side release — rejected

Against `shard` in `m2-scan`, the dispatch-side release costs `fillrandom`
5.5 % on `mcs_accordin` (209.682 against 221.859 Kops/s) and 4.8 % on
`mcs_tas_accordin` (212.832 against 223.664), with every CV in those four
cells at or below 2.30 %, so the loss is outside the spread. `readrandom` is
flat to slightly below (0.996x and 0.934x, the second at CV 8.5 %),
streamcluster is flat (0.983x and 1.034x against CV 0.9–5.0 %), and the barrier
column of that session separates nothing. The dispatch pass does what it was
built to do — it takes over 1.08 M of 6.28 M releases from the notifier's
syscall and drops the flush miss rate from 47.8 % to 1.3 % — and still loses,
at the price of 1.18 M / 1.12 M flush calls a run, nearly all of them
contended — 1.12 M of 1.16 M in the run whose extra counters are quoted
above — and a dispatch program of 1563 instructions against 342.

`scan-off` shows that the change also damaged the path it was added beside.
With the dispatch and timer passes switched off, leaving the notifier's flush
syscall as the only release, `fillrandom` on the same binaries reads 9.335 and
11.775 Kops/s — 0.042x and 0.053x of `shard`, which runs that syscall-only
release without the change. The counters say where the work went: `scan-off`
parks 282 k / 287 k waits against 6.63 M / 6.66 M, and roughly half of them
leave custody by expiry rather than by a release (145.8 k of 281.6 k, 149.2 k
of 286.9 k). A single run,
`fillrandom-192-scan-off-mcs_accordin-r1-a1`, reads parked 293520, flushed
142109, expired 151411, flush_calls 2544, flush_skipped 1146, and attributes
all 142109 releases to the syscall path. Streamcluster and `readrandom` under
`scan-off` are ordinary (0.586x / 0.547x of baseline; 0.943x / 0.945x), so the
collapse is confined to the workload where nearly every operation parks.

### Admission-walk cleanup — accepted

Against `shard` in `final`, the cleanup moves nothing. `fillrandom` reads
225.541 against 224.571 Kops/s on `mcs_accordin` and 224.764 against 226.756 on
`mcs_tas_accordin` (1.004x and 0.991x, every CV at or below 1.11 %).
`readrandom` moves in opposite directions on the two backends and stays inside
the same band (1.032x and 0.976x, CV at or below 1.61 %). Streamcluster is
0.997x and 1.028x against a CV of 3.8–8.3 %, and the barrier column separates
nothing at CV 54–124 %. The custody counters match arm for arm on all four
workloads. The cleanup is kept for what the verifier reports rather than for
throughput: it takes the dispatch program from 342 instructions to 256 and
leaves enqueue and the flush at their `46180cd` counts. `ded83aa` is the
accepted tip of `cv_admission`.

### Pure-mutex cost of the admission-side changes — carried

`mutex_bench` holds no condition variable, so nothing here parks in custody and
the counters stay at zero; what it measures is the admission path alone. Across
the two sessions the admission-side changes cost up to 7 % of `934bd1c`, and
nothing at all in some cells, with CV between 1.3 % and 7.8 %.

At 100 ns / 3000 ns the two sessions disagree on how much. `mutexbench-final`
reads 0.970x / 0.945x on `mcs_accordin_direct` and 0.961x / 0.960x on
`mcs_tas_accordin_direct` for `shard` and `tidy`; `mutexbench-attrib` puts the
same two commits within 1–2 % of its own baseline (0.990x / 0.993x and
0.984x / 0.999x). The spread of that point covers the difference.

At 300 ns / 3000 ns seven of the eight `shard` and `tidy` cells lose, six of
them by 3–7 %: 0.938x / 0.942x and 0.958x / 0.970x on `mcs_accordin_direct`,
0.931x / 0.941x for `mutexbench-attrib` on `mcs_tas_accordin_direct`. The
seventh, `tidy` in `mutexbench-final` on that backend, loses 1.7 % (0.983x).
Roughly half of the loss predates sharding: the `custody` arm of
`mutexbench-attrib` already reads 0.972x and 0.975x at `9520d7d`, on a commit
with no shards at all.

The lock itself is not slower. Mean hold time is 146.3–147.9 ns at the 100 ns
point and 387.7–391.6 ns at the 300 ns point, identical across arms to within a
nanosecond or two, and the whole difference sits in the estimated wait: at
300 ns / 3000 ns on `mcs_tas_accordin_direct`, 134.9 µs at baseline against
144.7 µs at `shard`, and on `mcs_accordin_direct` 123.5 µs against 128.9 µs.
The one cell that moves the other way, `shard` at 1.023x on
`mcs_tas_accordin_direct` in `mutexbench-final` — where its wait is also the
shorter one, 138.6 µs against 142.0 µs — does not reproduce: the same cell
reads 0.931x in `mutexbench-attrib`, and that baseline carries the session's
widest spread at CV 7.42 %.

The cost is carried rather than answered. Against it stand the `h1-shard`
moves on the workloads that do park: `fillrandom` at 3.77x and 3.73x of the
`custody` arm, streamcluster at 0.517x and 0.514x of the baseline's seconds.

### Two-lock fairness — no arm is unfair

Nothing in the series makes one group of threads pay for the other. Where the
two groups run the same parameters, every arm on both backends reads a Jain
index of 0.9999 or 1.0000, minimum 0.9997 over 40 runs. Where they differ, all
four arms sit in a band from 0.8576 to 0.9726, and the arms of the retained tip
are at its top rather than its bottom.

In the mild case sharding and the cleanup raise the index — 0.9726 and 0.9641
on `mcs_accordin_direct`, 0.9681 and 0.9666 on `mcs_tas_accordin_direct`,
against 0.9412 and 0.9469 at baseline — by taking throughput away from the
short-critical group B while leaving the long-critical group A where it was.
Group B falls from 0.618 to 0.561 and 0.546 Mops/s on `mcs_accordin_direct` and
from 0.685 to 0.598 and 0.594 on `mcs_tas_accordin_direct`, while group A moves
between 0.204 and 0.220 and between 0.224 and 0.234. That transfer is the whole
of the total-throughput loss in this case: 0.949x and 0.912x, 0.900x and
0.891x. A fairer split at a lower total is what the index is built to show, and
it is not an argument for the change.

In the extreme case the same commits gain a little on `mcs_accordin_direct`
(0.8850 and 0.8877 against 0.8718 at baseline and 0.8576 at `9520d7d`, total
throughput 1.034x and 1.026x) and match the baseline on
`mcs_tas_accordin_direct` (0.9422 and 0.9479 against 0.9445, 1.010x and
0.991x). The homogeneous case costs 0.92x to 0.98x of baseline on every arm
including `9520d7d`, in line with the single-lock 300 ns / 3000 ns cost of the
`mutexbench` sessions. Run-to-run spread is 1.7 % to 9.9 %, wide enough that
the extreme-case ordering carries less weight than the mild-case one, where the
Jain separation is larger than the spread of either arm.

### Per-thread fairness — an open cost of sharding

At 192 threads the sharded arms take a larger share of the operations into the
busier half of the threads than the unsharded ones, in all eight cells of that
thread count on both backends. At 1000 ns and 30000 ns the separation is
cleaner than the run-to-run range: on `mcs_accordin_direct` at 30000 ns,
`baseline` spans 0.6446–0.6543 and `custody` 0.6447–0.6507, against
0.6821–0.7033 for `shard` and 0.6865–0.7027 for `tidy`, with no overlap; the
same holds on `mcs_tas_accordin_direct` there (0.6434–0.6484 and 0.6362–0.6542
against 0.6772–0.6996 and 0.6823–0.7142) and at 1000 ns on both backends. The
split follows the sharding boundary and nothing else: in every one of those
cells the two unsharded arms sit together and the two sharded arms sit together
above them, `custody` with `baseline` and `tidy` with `shard`.

The effect scales with the thread count and with the critical section. At 48
threads no arm is separable from another — every mean sits between 0.5019 and
0.5155 except the 30000 ns `mcs_tas_accordin_direct` point, where the highest
figure belongs to `custody` rather than to a sharded arm. At 96 threads and
30000 ns it is 0.5732 against 0.6104 on `mcs_accordin_direct` and 0.5950
against 0.6232 on `mcs_tas_accordin_direct`, about 0.03 to 0.04. At 192 threads
and 30000 ns it is 0.6509 against 0.6952 and 0.6466 against 0.6900, and 0.7035
for `tidy` on `mcs_tas_accordin_direct`, so between 0.04 and 0.06.

It is the least-served thread that moves. At 192 threads and 30000 ns on
`mcs_accordin_direct` the mean per-thread minimum is 279 and 176 operations for
`baseline` and `custody` against 93 and 94 for `shard` and `tidy`, while the
maxima go the other way, 2212 and 2217 against 2537 and 2790. Throughput
carries no matching ordering: every 192-thread arm lands within about 5 % of
its baseline with the sign changing from cell to cell (0.947x to 1.016x), so
this is a distribution effect and not a slower lock.

The first reading of this was geometric: 48 online CPUs against 32 hash
shards leaves CPUs 0–15 sharing a shard with CPUs 32–47 while shards 16–31 hold
one CPU each, so a thread landing in a two-CPU shard would compete with twice
the population for the same grant rate. The `percpu` screen below refutes it —
a bank with one queue per CPU id, where no two CPUs share a queue, reproduces
the same spread — and that explanation is withdrawn.

### Per-CPU admission queues — screened, not adopted

**One run per cell.** Nothing below separates arms that differ by a few
percent, and no figure here carries the weight of the multi-repeat sessions.

The screen settles one question. `percpu` gives every CPU id its own queue, so
no two CPUs share one, and its fairness factor still matches the tip in every
cell: 0.5174 / 0.5234 against 0.5171 / 0.5240 at 100 ns, 0.5250 / 0.5315
against 0.5268 / 0.5419 at 300 ns, 0.5436 / 0.5482 against 0.5473 / 0.5644 at
1000 ns, and 0.7086 / 0.6974 against 0.7032 / 0.6793 at 30000 ns, where
`934bd1c` reads 0.6576 / 0.6520. Removing the shared shards changes nothing, so
the per-thread spread follows from granting one waiter per queue visit while
the queues hold unequal numbers of waiters, not from the shard geometry.

`own` trades that fairness for throughput. Probing the dispatching CPU's own
queue first raises the fairness factor from 0.5171 / 0.5240 to 0.6170 / 0.6172
at 100 ns, from 0.5268 / 0.5419 to 0.6392 / 0.6125 at 300 ns and from
0.5473 / 0.5644 to 0.5970 / 0.6195 at 1000 ns, with the least-served thread on
`mcs_accordin_direct` at 100 ns falling from 97470 to 58437 operations while
the best-served rises from 123799 to 229319. Only at 30000 ns does it sit
level with the tip, on the fairer side on one backend and the less fair side on
the other (0.6886 against 0.7032, 0.6910 against 0.6793). On the workloads that
park it is the fastest arm in the screen: `fillrandom` at 255.149 and 256.956
Kops/s against 225.140 and 221.413 for the tip, streamcluster at 93.261 s
against 114.653 on `mcs_accordin` — though on `mcs_tas_accordin` it reads
105.645 s against 102.243 for `percpu` — and `readrandom` between 1.016x and
0.954x of the tip. `percpu` on its own lands inside single-run noise on every
workload.

Neither commit is on `cv_admission`; the tip stays `ded83aa` and both stay on
`cv_percpu_queue`. The own-queue probe is recorded as a throughput-against-
fairness trade to be decided by whoever needs one or the other, not settled
here. The experiment it suggests is a bounded form — serve the dispatching
CPU's own queue at most a few times before advancing the rotation — which would
show whether the condvar-workload gain survives without the per-thread spread.
That variant is untested, and it would need the full multi-repeat sweep across
all seven workloads before any of this becomes a verdict.

## Machine state, not code: the reboot observation

The `baseline` and `custody` arms of `h2-tail` are the same binaries the two
2026-09-06 sessions measured, and the host was rebooted between the two sets
of runs.

| workload | backend | 2026-09-06 custody vs baseline | 2026-09-07 custody vs baseline |
|---|---|---:|---:|
| stream | mcs_accordin | 1.401x | 0.785x |
| stream | mcs_tas_accordin | 1.489x | 0.808x |
| barrier | mcs_accordin | 1.760x | 0.630x |
| barrier | mcs_tas_accordin | 1.574x | 0.876x |

Before the reboot, custody made streamcluster 1.40x slower and the barrier
1.76x slower than the baseline, and switching custody off recovered both
(0.712x and 0.980x). After the reboot the same libraries make streamcluster
0.785x — faster than the baseline — with custody on. The absolute figures
moved as well: the streamcluster baseline is 200.340 s before and 183.305 s
after; the custody arm parks at 30.1 k/s before and 58.3 k/s after, for the
same 8.4 M parks. The 2026-09-06 regression was a property of the machine at
that time, not of the change, which is why the acceptance rule anchors on a
baseline inside the same session.

## Caveats

- **Barrier is noisy.** Its CV reaches 100 % on some arms (`h1-shard/stream`:
  95.13 % and 101.35 %), and baseline CV is above 65 % in three sessions.
  Barrier is reported for completeness and no verdict rests on it alone.
- **The baseline hangs some `fillrandom` runs.** All five invalid attempts in
  the post-reboot LevelDB sessions are `baseline` (`934bd1c`) on
  `mcs_tas_accordin` `fillrandom`, each a 120 s timeout killed with signal 9.
  The affected cells retried to a valid sample, so no cell is missing, but the
  baseline anchor of those cells is drawn from the runs that completed.
- **`fillrandom` was bimodal on 2026-09-06.** The `custody` arm read
  28.786 / 62.232 / 28.709 Kops/s on `mcs_accordin` and 28.729 / 61.421 /
  28.792 on `mcs_tas_accordin`, CV 48 % and 48 %, straddling the two regimes
  within one arm. The post-reboot sessions do not show this on any arm
  (`fillrandom` CV at or below 4.4 % outside the baseline).
- **The `readrandom` custody counters are near zero everywhere** — about 200
  to 380 parks and one or two flush calls per 30 s run — so `readrandom`
  differences never reflect the volume of custody work, and the counter
  columns for it carry no information.
- **`slot-one` is not a one-slot custody run.** With `ACCORDIN_CV_SLOTS=1` the
  control block occupies the only slot, no thread claims one, and the arm
  records zero parks; it duplicates `slot-off` rather than measuring slot
  exhaustion under load.
- Three repeats per cell (four in `attrib-2`) is enough to separate the large
  moves and not enough to rank arms that differ by a few percent, which is why
  `readrandom` differences below about 5 % are treated as noise here.
- **The `percpu` screen is one run per cell.** It was run with reduced repeats
  to answer a single structural question quickly. Its throughput figures are
  indicative only; only the fairness ordering there, which is far larger than
  any spread the multi-repeat sessions show on the same metric, is treated as
  settled.
