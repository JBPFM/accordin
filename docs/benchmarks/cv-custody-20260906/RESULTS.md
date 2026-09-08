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
of that rotation. A third, `bf7080f` on branch `cv_owner_fair`, bounds that
preference: a CPU serves its own queue at most `ACCORDIN_OWN_LIMIT` times in a
row, then the deepest queue of its topology group, then its own queue again if
the group was empty, and the global rotation last. The groups come from the
NUMA node CPU lists chunked into eights, six groups of eight on this host —
the even CPUs 0–14, 16–30 and 32–46 and their odd counterparts. `8fe30c4`
follows it and restarts the grant count after a fallback own grant. Two further
commits on the same branch replace population with waiting time as the thing
the choice follows. `f1a5cf3` stamps every waiter filed into the admission bank
with its request time in the task's vtime field and serves the own queue only
while its head is no more than `ACCORDIN_OWN_SLACK_US` younger than the oldest
head of the topology group, otherwise the queue holding the oldest head, with
the count bound kept as an optional second limiter; it reads the heads through
the queue iterator. `5b25e15` then makes the bank a set of priority queues
ordered by that stamp — the lock-waiter insert and the flush's move both place
by age, so the head is the oldest by construction and the flush's head and tail
flags no longer place anything — and reads the heads through the kernel's
lockless queue peek where it resolves. All of these were later rebased onto
`ded83aa` and merged; the sessions below name their pre-rebase identities, and
the Decision section maps each to the commit that carries it now.

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

**The accepted tip of `cv_admission` is `4b3b0a8`.** The custody series itself
ended at `ded83aa`, the sharded admission queue plus the admission-walk
cleanup: neither the shared notify slot nor the dispatch-side release passed
validation. The own-queue work that followed was then rebased onto `ded83aa`
and the branch fast-forwarded, so the code tip is now `4b3b0a8`. The merge was
made on the strength of the single-repeat screens, at the user's decision,
without the multi-repeat confirmation the acceptance rule above asks for; that
confirmation is still owed.

**The shipped configuration.** One admission queue per CPU rather than
thirty-two hash shards. A CPU with a free slot serves its own queue while that
queue's head is within `ACCORDIN_OWN_SLACK_US` of the oldest head in its
topology group, otherwise the queue holding the oldest head; the groups are
eight CPUs of one NUMA node. The defaults at the tip are a slack of 100 µs, a
group size of 8 and `ACCORDIN_OWN_LIMIT` at 0, so no count bound limits
consecutive own-queue grants and the head ages decide alone. The bank is held
as age-ordered priority queues, so a queue head is the oldest request by
construction; the flush walks the custody queue from its head and its tail
placement flag is gone.

**The measured trade**, from the `peek` screen, one run per cell. `fillrandom`
gains about 11–12 % over `ded83aa` on both backends (250.956 against 224.397
Kops/s and 246.056 against 220.059). Per-thread fairness on a saturated mutex
lands between `ded83aa` and the unbounded own-queue preference: at 100 ns the
factor reads 0.5260 and 0.5528 against 0.5184 and 0.5230 for `ded83aa` in the
same session, and against 0.6275 and 0.6133 for the unbounded own-queue arm as
the `bounded` session measured it. `readrandom` and streamcluster stay inside
the noise of these single runs.

**Open items.**

- The multi-repeat confirmation of the tip across the seven workloads is still
  owed; every figure supporting the merge comes from one run per cell.
- The tip is unchanged at `4b3b0a8` after the streamcluster diagnosis below.
  The progress backstop stays on `cv_owner_fair`: it buys streamcluster 4–20 %
  and costs `readrandom` on `mcs_accordin` 12–13 % even with its timer
  unarmed, which is the code-volume question rather than a property of the
  rule, so it waits on that. The never-skip flush change is rejected — waiting
  for a pass in flight makes streamcluster 6–9 % slower and raises expiries by
  a third.
- `readrandom` on `mcs_accordin` loses 10–16 % to the volume of BPF text
  loaded into the kernel, reachable or not, with no data-layout, userspace or
  instruction-fetch cause found. It bounds what any further scheduler code can
  cost before it does anything.
- Cross-group service is reached only when both the own queue and the topology
  group have nothing, so a queue starved by its own group's traffic is served
  late.
- A released condvar waiter carries its park time, so an old park outranks the
  lock waiters of its group; the effect of that on lock-heavy phases is not
  measured.
- The flush's walk order interacts with a width cap: walking from the head
  hands over the oldest parks first, and no session has measured a capped
  width against the uncapped default.

**The map from the screens' commits to the tip's history.** The screened
commits were rebased, so the sessions above name their pre-rebase identities.

| in the sessions | on `cv_admission` | what it is |
|---|---|---|
| `1189963` | `158f798` | one admission queue per CPU |
| `cec374b` | `8f1bbcb` | own queue served first, unbounded |
| `bf7080f` | `e537dbb` | bounded own preference with topology groups |
| `8fe30c4` | `37d2ef2` | grant count restarted after a fallback grant |
| `f1a5cf3` | `a9a4338` | grants ordered by queue-head age |
| `5b25e15` | `0e92abe` | age-ordered bank read through the lockless peek |
| — | `4b3b0a8` | shipped defaults: no count bound, flush from the head |

- **`46180cd`, sharding with the home-CPU idle kick, is kept.** Branch
  `cv_admission` was reset to it and then carried the cleanup commit `ded83aa`
  on top, which the `final` session measures as neutral. `ded83aa` was the
  accepted tip of the custody series and is the base of everything above.
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
  open cost of sharding that the per-CPU queues alone do not remove. The
  own-queue screens that follow answer it instead: the unbounded probe buys
  throughput at a clear fairness cost, the count bound gives much of it back,
  and the age rule of the shipped tip gives back more while keeping most of
  the throughput. Sharding itself is kept regardless: on the same commits
  `fillrandom` runs at 3.77x and 3.73x of the `custody` arm and streamcluster
  at 0.517x and 0.514x of the baseline's seconds, so the gains on the workloads
  that park dominate the microbenchmark cost.
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
  2026-09-07 and 06:39 UTC on 2026-09-08, with the host up for 12 h or more.
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

`tidy` is `ded83aa`, which was the tip of `cv_admission` when this session ran
and is now the base of the merged own-queue work described in the Decision
section: `46180cd` plus the admission-walk cleanup and nothing else — an early stop when a slot is lost, an
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

### `bounded` — bounded own-queue preference, single repeat

`bf7080f` under two bounds, against `934bd1c`, the retained tip and the
unbounded own-queue probe. **One run per cell again**, so the throughput
columns scatter and only differences far larger than that scatter mean
anything.

Per-thread fairness, 192 threads, one repeat:

| critical | arm | commit | env | backend | Mops/s | rel | fairness | min ops | max ops |
|---|---|---|---|---|---:|---:|---:|---:|---:|
| 100 ns | baseline | `934bd1c` | — | mcs_accordin_direct | 2.624 | 1.000x | 0.5113 | 100913 | 118634 |
| 100 ns | tidy | `ded83aa` | — | mcs_accordin_direct | 2.599 | 0.991x | 0.5187 | 90092 | 126102 |
| 100 ns | own | `cec374b` | — | mcs_accordin_direct | 2.544 | 0.970x | 0.6275 | 49823 | 217888 |
| 100 ns | k2 | `bf7080f` | `OWN_LIMIT=2` | mcs_accordin_direct | 2.374 | 0.905x | 0.5530 | 80033 | 127451 |
| 100 ns | k4 | `bf7080f` | `OWN_LIMIT=4` | mcs_accordin_direct | 2.596 | 0.989x | 0.5552 | 86049 | 147997 |
| 100 ns | baseline | `934bd1c` | — | mcs_tas_accordin_direct | 2.468 | 1.000x | 0.5181 | 94075 | 116936 |
| 100 ns | tidy | `ded83aa` | — | mcs_tas_accordin_direct | 2.324 | 0.942x | 0.5221 | 83330 | 117125 |
| 100 ns | own | `cec374b` | — | mcs_tas_accordin_direct | 2.199 | 0.891x | 0.6133 | 45499 | 172849 |
| 100 ns | k2 | `bf7080f` | `OWN_LIMIT=2` | mcs_tas_accordin_direct | 2.329 | 0.944x | 0.5415 | 80449 | 132769 |
| 100 ns | k4 | `bf7080f` | `OWN_LIMIT=4` | mcs_tas_accordin_direct | 2.395 | 0.971x | 0.5587 | 80030 | 138892 |
| 300 ns | baseline | `934bd1c` | — | mcs_accordin_direct | 1.490 | 1.000x | 0.5183 | 54730 | 70888 |
| 300 ns | tidy | `ded83aa` | — | mcs_accordin_direct | 1.489 | 0.999x | 0.5290 | 51287 | 73122 |
| 300 ns | own | `cec374b` | — | mcs_accordin_direct | 1.377 | 0.924x | 0.6180 | 30420 | 118765 |
| 300 ns | k2 | `bf7080f` | `OWN_LIMIT=2` | mcs_accordin_direct | 1.411 | 0.947x | 0.5523 | 46329 | 76236 |
| 300 ns | k4 | `bf7080f` | `OWN_LIMIT=4` | mcs_accordin_direct | 1.390 | 0.933x | 0.5801 | 39862 | 97990 |
| 300 ns | baseline | `934bd1c` | — | mcs_tas_accordin_direct | 1.373 | 1.000x | 0.5285 | 48995 | 70823 |
| 300 ns | tidy | `ded83aa` | — | mcs_tas_accordin_direct | 1.358 | 0.988x | 0.5382 | 42685 | 73249 |
| 300 ns | own | `cec374b` | — | mcs_tas_accordin_direct | 1.347 | 0.981x | 0.6461 | 26432 | 114074 |
| 300 ns | k2 | `bf7080f` | `OWN_LIMIT=2` | mcs_tas_accordin_direct | 1.332 | 0.970x | 0.5697 | 36340 | 91484 |
| 300 ns | k4 | `bf7080f` | `OWN_LIMIT=4` | mcs_tas_accordin_direct | 1.223 | 0.891x | 0.5593 | 36813 | 82703 |
| 1000 ns | baseline | `934bd1c` | — | mcs_accordin_direct | 0.548 | 1.000x | 0.5264 | 19014 | 27240 |
| 1000 ns | tidy | `ded83aa` | — | mcs_accordin_direct | 0.575 | 1.048x | 0.5469 | 16933 | 32936 |
| 1000 ns | own | `cec374b` | — | mcs_accordin_direct | 0.581 | 1.059x | 0.6138 | 13610 | 51540 |
| 1000 ns | k2 | `bf7080f` | `OWN_LIMIT=2` | mcs_accordin_direct | 0.463 | 0.844x | 0.5762 | 12267 | 29489 |
| 1000 ns | k4 | `bf7080f` | `OWN_LIMIT=4` | mcs_accordin_direct | 0.587 | 1.072x | 0.5666 | 17944 | 36327 |
| 1000 ns | baseline | `934bd1c` | — | mcs_tas_accordin_direct | 0.598 | 1.000x | 0.5539 | 17806 | 39379 |
| 1000 ns | tidy | `ded83aa` | — | mcs_tas_accordin_direct | 0.568 | 0.951x | 0.5748 | 14515 | 40446 |
| 1000 ns | own | `cec374b` | — | mcs_tas_accordin_direct | 0.476 | 0.796x | 0.6395 | 6649 | 44360 |
| 1000 ns | k2 | `bf7080f` | `OWN_LIMIT=2` | mcs_tas_accordin_direct | 0.618 | 1.033x | 0.5817 | 14984 | 46256 |
| 1000 ns | k4 | `bf7080f` | `OWN_LIMIT=4` | mcs_tas_accordin_direct | 0.537 | 0.897x | 0.6015 | 13570 | 50852 |
| 30000 ns | baseline | `934bd1c` | — | mcs_accordin_direct | 0.021 | 1.000x | 0.6496 | 210 | 1953 |
| 30000 ns | tidy | `ded83aa` | — | mcs_accordin_direct | 0.023 | 1.083x | 0.7206 | 23 | 2736 |
| 30000 ns | own | `cec374b` | — | mcs_accordin_direct | 0.020 | 0.967x | 0.6986 | 115 | 2687 |
| 30000 ns | k2 | `bf7080f` | `OWN_LIMIT=2` | mcs_accordin_direct | 0.023 | 1.072x | 0.6780 | 62 | 2448 |
| 30000 ns | k4 | `bf7080f` | `OWN_LIMIT=4` | mcs_accordin_direct | 0.022 | 1.025x | 0.6949 | 109 | 2293 |
| 30000 ns | baseline | `934bd1c` | — | mcs_tas_accordin_direct | 0.026 | 1.000x | 0.6569 | 201 | 2211 |
| 30000 ns | tidy | `ded83aa` | — | mcs_tas_accordin_direct | 0.026 | 1.002x | 0.6901 | 147 | 3022 |
| 30000 ns | own | `cec374b` | — | mcs_tas_accordin_direct | 0.026 | 1.002x | 0.6710 | 69 | 2393 |
| 30000 ns | k2 | `bf7080f` | `OWN_LIMIT=2` | mcs_tas_accordin_direct | 0.026 | 1.003x | 0.6795 | 99 | 2719 |
| 30000 ns | k4 | `bf7080f` | `OWN_LIMIT=4` | mcs_tas_accordin_direct | 0.026 | 0.994x | 0.6829 | 218 | 2487 |

LevelDB, one repeat (Kops/s, `rel` against `baseline`):

| arm | commit | env | backend | fill Kops/s | rel | read Kops/s | rel |
|---|---|---|---|---:|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 53.981 | 1.000x | 1337.426 | 1.000x |
| tidy | `ded83aa` | — | mcs_accordin | 226.554 | 4.197x | 1313.132 | 0.982x |
| own | `cec374b` | — | mcs_accordin | 250.395 | 4.639x | 1295.567 | 0.969x |
| k2 | `bf7080f` | `OWN_LIMIT=2` | mcs_accordin | 255.004 | 4.724x | 1283.480 | 0.960x |
| k4 | `bf7080f` | `OWN_LIMIT=4` | mcs_accordin | 258.525 | 4.789x | 1277.318 | 0.955x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 64.561 | 1.000x | 1104.809 | 1.000x |
| tidy | `ded83aa` | — | mcs_tas_accordin | 223.281 | 3.458x | 1084.192 | 0.981x |
| own | `cec374b` | — | mcs_tas_accordin | 253.079 | 3.920x | 1082.289 | 0.980x |
| k2 | `bf7080f` | `OWN_LIMIT=2` | mcs_tas_accordin | 252.587 | 3.912x | 1036.534 | 0.938x |
| k4 | `bf7080f` | `OWN_LIMIT=4` | mcs_tas_accordin | 261.155 | 4.045x | 1066.425 | 0.965x |

Counters, `fillrandom`: the bounded arms park what the unbounded one parks —
7.61 M / 7.52 M for `k2` and 7.72 M / 7.79 M for `k4` against 7.47 M / 7.53 M
for `own` and 6.78 M / 6.65 M for the tip — with flush calls at 175 k–182 k
against 171 k / 172 k and 143 k / 138 k, at a miss rate near 52 % for the three
own-queue arms against 48 % for the tip.

streamcluster, one repeat, seconds (lower is better):

| arm | commit | env | backend | stream s | rel |
|---|---|---|---|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 151.226 | 1.000x |
| tidy | `ded83aa` | — | mcs_accordin | 96.015 | 0.635x |
| own | `cec374b` | — | mcs_accordin | 93.407 | 0.618x |
| k2 | `bf7080f` | `OWN_LIMIT=2` | mcs_accordin | 92.745 | 0.613x |
| k4 | `bf7080f` | `OWN_LIMIT=4` | mcs_accordin | 92.820 | 0.614x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 135.782 | 1.000x |
| tidy | `ded83aa` | — | mcs_tas_accordin | 92.109 | 0.678x |
| own | `cec374b` | — | mcs_tas_accordin | 96.301 | 0.709x |
| k2 | `bf7080f` | `OWN_LIMIT=2` | mcs_tas_accordin | 92.391 | 0.680x |
| k4 | `bf7080f` | `OWN_LIMIT=4` | mcs_tas_accordin | 89.021 | 0.656x |

All four arms park 8.43 M waits with about 44.4 k flush calls and expire
45.4 k–49.7 k of them; nothing in the counters separates them.

### `age` — grants ordered by queue-head age, single repeat

`f1a5cf3` under two slack settings, against `934bd1c`, the retained tip and the
bounded-count arm. **One run per cell.** `age100` and `age0` also carry
`ACCORDIN_OWN_LIMIT=0`, so the age rule is the only limiter in those arms.

Per-thread fairness, 192 threads, one repeat:

| critical | arm | commit | env | backend | Mops/s | rel | fairness | min ops | max ops |
|---|---|---|---|---|---:|---:|---:|---:|---:|
| 100 ns | baseline | `934bd1c` | — | mcs_accordin_direct | 2.597 | 1.000x | 0.5134 | 99472 | 117205 |
| 100 ns | tidy | `ded83aa` | — | mcs_accordin_direct | 2.644 | 1.018x | 0.5175 | 95166 | 123060 |
| 100 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_accordin_direct | 2.668 | 1.027x | 0.5472 | 80845 | 132838 |
| 100 ns | age100 | `f1a5cf3` | `SLACK_US=100` | mcs_accordin_direct | 2.576 | 0.992x | 0.5299 | 88100 | 124986 |
| 100 ns | age0 | `f1a5cf3` | `SLACK_US=0` | mcs_accordin_direct | 2.547 | 0.981x | 0.5270 | 92652 | 124677 |
| 100 ns | baseline | `934bd1c` | — | mcs_tas_accordin_direct | 2.242 | 1.000x | 0.5167 | 83423 | 115925 |
| 100 ns | tidy | `ded83aa` | — | mcs_tas_accordin_direct | 2.215 | 0.988x | 0.5253 | 79108 | 112588 |
| 100 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_tas_accordin_direct | 2.264 | 1.010x | 0.5936 | 62074 | 140335 |
| 100 ns | age100 | `f1a5cf3` | `SLACK_US=100` | mcs_tas_accordin_direct | 2.315 | 1.032x | 0.5472 | 72377 | 119431 |
| 100 ns | age0 | `f1a5cf3` | `SLACK_US=0` | mcs_tas_accordin_direct | 2.408 | 1.074x | 0.5447 | 75816 | 125932 |
| 300 ns | baseline | `934bd1c` | — | mcs_accordin_direct | 1.494 | 1.000x | 0.5191 | 53883 | 71198 |
| 300 ns | tidy | `ded83aa` | — | mcs_accordin_direct | 0.753 | 0.504x | 0.5426 | 23997 | 40156 |
| 300 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_accordin_direct | 1.436 | 0.961x | 0.5671 | 35516 | 82359 |
| 300 ns | age100 | `f1a5cf3` | `SLACK_US=100` | mcs_accordin_direct | 1.480 | 0.991x | 0.5301 | 50899 | 72574 |
| 300 ns | age0 | `f1a5cf3` | `SLACK_US=0` | mcs_accordin_direct | 1.422 | 0.952x | 0.5487 | 46818 | 76240 |
| 300 ns | baseline | `934bd1c` | — | mcs_tas_accordin_direct | 1.073 | 1.000x | 0.5439 | 35524 | 64063 |
| 300 ns | tidy | `ded83aa` | — | mcs_tas_accordin_direct | 1.273 | 1.187x | 0.5410 | 39175 | 66905 |
| 300 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_tas_accordin_direct | 1.374 | 1.280x | 0.5626 | 41084 | 81434 |
| 300 ns | age100 | `f1a5cf3` | `SLACK_US=100` | mcs_tas_accordin_direct | 1.340 | 1.249x | 0.5405 | 46690 | 79781 |
| 300 ns | age0 | `f1a5cf3` | `SLACK_US=0` | mcs_tas_accordin_direct | 1.289 | 1.201x | 0.5479 | 37726 | 70529 |
| 1000 ns | baseline | `934bd1c` | — | mcs_accordin_direct | 0.619 | 1.000x | 0.5279 | 20587 | 31163 |
| 1000 ns | tidy | `ded83aa` | — | mcs_accordin_direct | 0.565 | 0.913x | 0.5470 | 15161 | 31551 |
| 1000 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_accordin_direct | 0.637 | 1.029x | 0.5471 | 17768 | 34589 |
| 1000 ns | age100 | `f1a5cf3` | `SLACK_US=100` | mcs_accordin_direct | 0.576 | 0.931x | 0.5639 | 14843 | 31677 |
| 1000 ns | age0 | `f1a5cf3` | `SLACK_US=0` | mcs_accordin_direct | 0.553 | 0.894x | 0.5718 | 14751 | 33881 |
| 1000 ns | baseline | `934bd1c` | — | mcs_tas_accordin_direct | 0.590 | 1.000x | 0.5494 | 19427 | 38634 |
| 1000 ns | tidy | `ded83aa` | — | mcs_tas_accordin_direct | 0.537 | 0.911x | 0.5782 | 11741 | 36805 |
| 1000 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_tas_accordin_direct | 0.581 | 0.985x | 0.5706 | 14104 | 38084 |
| 1000 ns | age100 | `f1a5cf3` | `SLACK_US=100` | mcs_tas_accordin_direct | 0.607 | 1.029x | 0.5927 | 16546 | 41356 |
| 1000 ns | age0 | `f1a5cf3` | `SLACK_US=0` | mcs_tas_accordin_direct | 0.567 | 0.962x | 0.5760 | 15379 | 37976 |
| 30000 ns | baseline | `934bd1c` | — | mcs_accordin_direct | 0.022 | 1.000x | 0.6649 | 216 | 2026 |
| 30000 ns | tidy | `ded83aa` | — | mcs_accordin_direct | 0.024 | 1.094x | 0.6923 | 163 | 3149 |
| 30000 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_accordin_direct | 0.022 | 1.035x | 0.6834 | 145 | 2569 |
| 30000 ns | age100 | `f1a5cf3` | `SLACK_US=100` | mcs_accordin_direct | 0.021 | 0.979x | 0.6695 | 241 | 1906 |
| 30000 ns | age0 | `f1a5cf3` | `SLACK_US=0` | mcs_accordin_direct | 0.024 | 1.099x | 0.6502 | 147 | 2012 |
| 30000 ns | baseline | `934bd1c` | — | mcs_tas_accordin_direct | 0.027 | 1.000x | 0.6475 | 286 | 2792 |
| 30000 ns | tidy | `ded83aa` | — | mcs_tas_accordin_direct | 0.026 | 0.996x | 0.6958 | 137 | 3041 |
| 30000 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_tas_accordin_direct | 0.027 | 1.011x | 0.6662 | 214 | 2455 |
| 30000 ns | age100 | `f1a5cf3` | `SLACK_US=100` | mcs_tas_accordin_direct | 0.026 | 0.996x | 0.6508 | 222 | 2306 |
| 30000 ns | age0 | `f1a5cf3` | `SLACK_US=0` | mcs_tas_accordin_direct | 0.027 | 1.005x | 0.6668 | 312 | 2671 |

Two cells of this table are outliers rather than measurements of the arm: the
tip at 300 ns on `mcs_accordin_direct` ran at 0.753 Mops/s, about half of every
other arm at that point, and the `mcs_tas_accordin_direct` baseline at the same
point reads 1.073 Mops/s against 1.36–1.37 in the neighbouring sessions, which
is what puts the other arms of that row above 1.18x.

LevelDB, one repeat (Kops/s, `rel` against `baseline`):

| arm | commit | env | backend | fill Kops/s | rel | read Kops/s | rel |
|---|---|---|---|---:|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 59.116 | 1.000x | 1385.868 | 1.000x |
| tidy | `ded83aa` | — | mcs_accordin | 225.789 | 3.819x | 1319.004 | 0.952x |
| k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_accordin | 252.781 | 4.276x | 1312.078 | 0.947x |
| age100 | `f1a5cf3` | `SLACK_US=100` | mcs_accordin | 246.502 | 4.170x | 1321.559 | 0.954x |
| age0 | `f1a5cf3` | `SLACK_US=0` | mcs_accordin | 245.468 | 4.152x | 1298.893 | 0.937x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 65.531 | 1.000x | 1101.028 | 1.000x |
| tidy | `ded83aa` | — | mcs_tas_accordin | 224.671 | 3.428x | 1063.606 | 0.966x |
| k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_tas_accordin | 256.827 | 3.919x | 1077.011 | 0.978x |
| age100 | `f1a5cf3` | `SLACK_US=100` | mcs_tas_accordin | 244.936 | 3.738x | 1115.740 | 1.013x |
| age0 | `f1a5cf3` | `SLACK_US=0` | mcs_tas_accordin | 248.697 | 3.795x | 1074.985 | 0.976x |

One attempt was invalid, `baseline` on `mcs_accordin` `fillrandom`, a 120 s
timeout. Counters, `fillrandom`: the age arms park 7.33–7.41 M waits against
7.55 M / 7.63 M for `k2` and 6.75 M / 6.69 M for the tip, with flush calls at
162 k–168 k against 174 k / 181 k and 144 k / 145 k.

streamcluster, one repeat, seconds:

| arm | commit | env | backend | stream s | rel |
|---|---|---|---|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 155.576 | 1.000x |
| tidy | `ded83aa` | — | mcs_accordin | 96.215 | 0.618x |
| k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_accordin | 89.506 | 0.575x |
| age100 | `f1a5cf3` | `SLACK_US=100` | mcs_accordin | 92.298 | 0.593x |
| age0 | `f1a5cf3` | `SLACK_US=0` | mcs_accordin | 95.060 | 0.611x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 149.380 | 1.000x |
| tidy | `ded83aa` | — | mcs_tas_accordin | 91.733 | 0.614x |
| k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_tas_accordin | 97.709 | 0.654x |
| age100 | `f1a5cf3` | `SLACK_US=100` | mcs_tas_accordin | 90.075 | 0.603x |
| age0 | `f1a5cf3` | `SLACK_US=0` | mcs_tas_accordin | 95.227 | 0.637x |

### `peek` — age-ordered bank read without locking, single repeat

`5b25e15` under the same two slack settings and the same reference arms.
**One run per cell.**

Per-thread fairness, 192 threads, one repeat:

| critical | arm | commit | env | backend | Mops/s | rel | fairness | min ops | max ops |
|---|---|---|---|---|---:|---:|---:|---:|---:|
| 100 ns | baseline | `934bd1c` | — | mcs_accordin_direct | 2.753 | 1.000x | 0.5127 | 102962 | 125528 |
| 100 ns | tidy | `ded83aa` | — | mcs_accordin_direct | 2.651 | 0.963x | 0.5184 | 95571 | 123284 |
| 100 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_accordin_direct | 2.516 | 0.914x | 0.5457 | 80059 | 125010 |
| 100 ns | peek100 | `5b25e15` | `SLACK_US=100` | mcs_accordin_direct | 2.664 | 0.968x | 0.5260 | 90039 | 124290 |
| 100 ns | peek0 | `5b25e15` | `SLACK_US=0` | mcs_accordin_direct | 2.598 | 0.944x | 0.5217 | 92849 | 122966 |
| 100 ns | baseline | `934bd1c` | — | mcs_tas_accordin_direct | 2.322 | 1.000x | 0.5203 | 86906 | 114431 |
| 100 ns | tidy | `ded83aa` | — | mcs_tas_accordin_direct | 2.294 | 0.988x | 0.5230 | 80565 | 110489 |
| 100 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_tas_accordin_direct | 2.353 | 1.014x | 0.5628 | 74859 | 170703 |
| 100 ns | peek100 | `5b25e15` | `SLACK_US=100` | mcs_tas_accordin_direct | 2.327 | 1.002x | 0.5528 | 73329 | 123577 |
| 100 ns | peek0 | `5b25e15` | `SLACK_US=0` | mcs_tas_accordin_direct | 2.353 | 1.013x | 0.5529 | 78516 | 121213 |
| 300 ns | baseline | `934bd1c` | — | mcs_accordin_direct | 1.516 | 1.000x | 0.5170 | 56769 | 70392 |
| 300 ns | tidy | `ded83aa` | — | mcs_accordin_direct | 1.477 | 0.974x | 0.5344 | 49580 | 76561 |
| 300 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_accordin_direct | 1.489 | 0.982x | 0.5629 | 47779 | 102935 |
| 300 ns | peek100 | `5b25e15` | `SLACK_US=100` | mcs_accordin_direct | 1.474 | 0.973x | 0.5535 | 43552 | 77463 |
| 300 ns | peek0 | `5b25e15` | `SLACK_US=0` | mcs_accordin_direct | 1.478 | 0.975x | 0.5505 | 45094 | 77291 |
| 300 ns | baseline | `934bd1c` | — | mcs_tas_accordin_direct | 1.360 | 1.000x | 0.5299 | 48272 | 72057 |
| 300 ns | tidy | `ded83aa` | — | mcs_tas_accordin_direct | 1.357 | 0.998x | 0.5350 | 42222 | 74929 |
| 300 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_tas_accordin_direct | 1.440 | 1.059x | 0.5516 | 48669 | 87569 |
| 300 ns | peek100 | `5b25e15` | `SLACK_US=100` | mcs_tas_accordin_direct | 1.325 | 0.974x | 0.5507 | 42055 | 79272 |
| 300 ns | peek0 | `5b25e15` | `SLACK_US=0` | mcs_tas_accordin_direct | 1.560 | 1.147x | 0.5505 | 49567 | 78680 |
| 1000 ns | baseline | `934bd1c` | — | mcs_accordin_direct | 0.603 | 1.000x | 0.5271 | 19856 | 29833 |
| 1000 ns | tidy | `ded83aa` | — | mcs_accordin_direct | 0.600 | 0.994x | 0.5491 | 15641 | 32519 |
| 1000 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_accordin_direct | 0.660 | 1.093x | 0.5454 | 20919 | 34620 |
| 1000 ns | peek100 | `5b25e15` | `SLACK_US=100` | mcs_accordin_direct | 0.655 | 1.085x | 0.5782 | 17767 | 41078 |
| 1000 ns | peek0 | `5b25e15` | `SLACK_US=0` | mcs_accordin_direct | 0.633 | 1.049x | 0.5596 | 18847 | 36920 |
| 1000 ns | baseline | `934bd1c` | — | mcs_tas_accordin_direct | 0.604 | 1.000x | 0.5485 | 18717 | 34694 |
| 1000 ns | tidy | `ded83aa` | — | mcs_tas_accordin_direct | 0.640 | 1.059x | 0.5592 | 16163 | 40983 |
| 1000 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_tas_accordin_direct | 0.654 | 1.083x | 0.5769 | 16753 | 45372 |
| 1000 ns | peek100 | `5b25e15` | `SLACK_US=100` | mcs_tas_accordin_direct | 0.606 | 1.003x | 0.5780 | 16175 | 37423 |
| 1000 ns | peek0 | `5b25e15` | `SLACK_US=0` | mcs_tas_accordin_direct | 0.592 | 0.980x | 0.5940 | 11392 | 39009 |
| 30000 ns | baseline | `934bd1c` | — | mcs_accordin_direct | 0.023 | 1.000x | 0.6719 | 237 | 2271 |
| 30000 ns | tidy | `ded83aa` | — | mcs_accordin_direct | 0.022 | 0.977x | 0.7162 | 114 | 2379 |
| 30000 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_accordin_direct | 0.022 | 0.994x | 0.6854 | 146 | 2216 |
| 30000 ns | peek100 | `5b25e15` | `SLACK_US=100` | mcs_accordin_direct | 0.025 | 1.129x | 0.6337 | 356 | 2107 |
| 30000 ns | peek0 | `5b25e15` | `SLACK_US=0` | mcs_accordin_direct | 0.023 | 1.026x | 0.6558 | 193 | 2144 |
| 30000 ns | baseline | `934bd1c` | — | mcs_tas_accordin_direct | 0.026 | 1.000x | 0.6655 | 275 | 2664 |
| 30000 ns | tidy | `ded83aa` | — | mcs_tas_accordin_direct | 0.027 | 1.014x | 0.6871 | 129 | 3042 |
| 30000 ns | k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_tas_accordin_direct | 0.027 | 1.012x | 0.6838 | 145 | 4150 |
| 30000 ns | peek100 | `5b25e15` | `SLACK_US=100` | mcs_tas_accordin_direct | 0.027 | 1.012x | 0.6573 | 196 | 2767 |
| 30000 ns | peek0 | `5b25e15` | `SLACK_US=0` | mcs_tas_accordin_direct | 0.026 | 1.007x | 0.6715 | 150 | 2764 |

LevelDB, one repeat (Kops/s, `rel` against `baseline`):

| arm | commit | env | backend | fill Kops/s | rel | read Kops/s | rel |
|---|---|---|---|---:|---:|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 56.601 | 1.000x | 1313.018 | 1.000x |
| tidy | `ded83aa` | — | mcs_accordin | 224.397 | 3.965x | 1306.754 | 0.995x |
| k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_accordin | 252.301 | 4.458x | 1296.857 | 0.988x |
| peek100 | `5b25e15` | `SLACK_US=100` | mcs_accordin | 250.956 | 4.434x | 1287.813 | 0.981x |
| peek0 | `5b25e15` | `SLACK_US=0` | mcs_accordin | 245.789 | 4.342x | 1311.780 | 0.999x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 51.059 | 1.000x | 1115.323 | 1.000x |
| tidy | `ded83aa` | — | mcs_tas_accordin | 220.059 | 4.310x | 999.062 | 0.896x |
| k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_tas_accordin | 261.140 | 5.114x | 1092.576 | 0.980x |
| peek100 | `5b25e15` | `SLACK_US=100` | mcs_tas_accordin | 246.056 | 4.819x | 1094.471 | 0.981x |
| peek0 | `5b25e15` | `SLACK_US=0` | mcs_tas_accordin | 245.116 | 4.801x | 1105.357 | 0.991x |

The tip's `readrandom` on `mcs_tas_accordin` reads 0.896x here, well below the
0.95x–0.99x it holds in every other session, and is an outlier of this single
run rather than a property of the arm. Counters, `fillrandom`: the peek arms
park 7.29–7.49 M waits with 162 k–169 k flush calls, between the tip
(6.70 M / 6.56 M, 145 k / 144 k) and `k2` (7.52 M / 7.74 M, 174 k / 184 k).

streamcluster, one repeat, seconds:

| arm | commit | env | backend | stream s | rel |
|---|---|---|---|---:|---:|
| baseline | `934bd1c` | — | mcs_accordin | 183.134 | 1.000x |
| tidy | `ded83aa` | — | mcs_accordin | 90.224 | 0.493x |
| k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_accordin | 92.274 | 0.504x |
| peek100 | `5b25e15` | `SLACK_US=100` | mcs_accordin | 97.855 | 0.534x |
| peek0 | `5b25e15` | `SLACK_US=0` | mcs_accordin | 101.529 | 0.554x |
| baseline | `934bd1c` | — | mcs_tas_accordin | 176.156 | 1.000x |
| tidy | `ded83aa` | — | mcs_tas_accordin | 105.891 | 0.601x |
| k2 | `8fe30c4` | `OWN_LIMIT=2` | mcs_tas_accordin | 91.212 | 0.518x |
| peek100 | `5b25e15` | `SLACK_US=100` | mcs_tas_accordin | 89.691 | 0.509x |
| peek0 | `5b25e15` | `SLACK_US=0` | mcs_tas_accordin | 92.486 | 0.525x |

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
leaves enqueue and the flush at their `46180cd` counts. `ded83aa` closed the
custody series as the accepted tip; the Decision section records the merge that
has since moved the branch past it.

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

Neither commit was on `cv_admission` when this was written; the tip was
`ded83aa` and both stayed on `cv_percpu_queue`. Both are in the branch's
history now, rebased, as the Decision section records. The own-queue probe is recorded as a throughput-against-
fairness trade to be decided by whoever needs one or the other, not settled
here. The experiment it suggests is a bounded form — serve the dispatching
CPU's own queue at most a few times before advancing the rotation — which would
show whether the condvar-workload gain survives without the per-thread spread.
That variant is untested, and it would need the full multi-repeat sweep across
all seven workloads before any of this becomes a verdict.

### Bounded own-queue preference — promising, not confirmed

**One run per cell.** The fairness ordering below is larger than the scatter of
the multi-repeat sessions on the same metric; the throughput figures are not.

Bounding the preference gives back much of what `cec374b` took. At 100 ns the
factor reads 0.5530 and 0.5552 for `k2` and `k4` on `mcs_accordin_direct` and
0.5415 and 0.5587 on `mcs_tas_accordin_direct`, against 0.6275 and 0.6133 for
`own` and 0.5187 and 0.5221 for the tip; at 300 ns, 0.5523 and 0.5801, and
0.5697 and 0.5593, against 0.6180 and 0.6461 for `own` and 0.5290 and 0.5382
for the tip. That is between two fifths and four fifths of the gap in a single
run, most cells around two thirds. The least-served thread moves with it: at
100 ns the two bounds record between 80030 and 86049 operations, against 49823
and 45499 for `own`, 90092 and 83330 for the tip, and 100913 and 94075 for
`934bd1c`. `k2` and `k4` are not separable
from each other in one run, and at 30000 ns nothing is separable from anything
(0.6496 to 0.7206 across all five arms).

The throughput side keeps the unbounded arm's gain. `fillrandom` reads 255.004
and 252.587 Kops/s for `k2` and 258.525 and 261.155 for `k4`, against 250.395
and 253.079 for `own` and 226.554 and 223.281 for the tip. `readrandom` sits at
0.960x / 0.955x and 0.938x / 0.965x of `934bd1c` for the bounded arms against
0.982x / 0.981x for the tip, a single-run difference of a few percent on a
workload that parks about 200 waits. Streamcluster no longer separates the
shard-family arms at all — 89.021 to 96.301 s for the four of them against
151.226 and 135.782 s at baseline — so the streamcluster advantage `own` showed
in the `percpu` screen did not reproduce here.

**Why a bound helps, by the design's own argument.** For a group of m CPUs,
the instantaneous service ratio between a lone thread and a thread sharing a
queue of population n0 is k·n0/(k + m − 1), and the crowded queue drains at
(m − 1)r/(k + 1), so a smaller bound is fairer by construction and the
unbounded case is the limit where the ratio grows with n0 without bound. The
measured factor stops falling at 0.55–0.58 rather than returning to the tip's
0.52, which the bound cannot explain on its own; the remaining spread is
attributed to per-CPU slot turnover differing between CPUs and to service
crossing group boundaries, neither of which k controls. That attribution is
untested.

The walk is not free: the commit's own validation note reports the dispatch
program at 8159 instructions with enqueue unchanged at 415, against 256 and 415
at the tip. Adoption waits on a multi-repeat confirm of `8fe30c4`, which
restarts the grant count after a fallback own grant and so changes the cadence
this screen measured; at the time of writing `bf7080f` and `8fe30c4` stayed on
`cv_owner_fair` and the tip of `cv_admission` was `ded83aa`. Both were later
merged, in rebased form, with the count bound off by default.

### Age-ordered grants — the best screen so far, still unconfirmed

**One run per cell in both sessions.** The two screens agree with each other,
which is the only reason the reading below is offered at all.

Choosing by waiting time instead of by population moves the fairness factor off
the bounded-count level and back toward the tip. At 100 ns the age arms read
0.5299 and 0.5270 on `mcs_accordin_direct` and 0.5472 and 0.5447 on
`mcs_tas_accordin_direct`, and the peek arms 0.5260 and 0.5217 and 0.5528 and
0.5529, against 0.5457–0.5936 for `k2` in the same two sessions and
0.5175–0.5253 for the tip. At 300 ns they read 0.5301–0.5535 and 0.5405–0.5507,
against 0.5516–0.5671 for `k2` and 0.5344–0.5426 for the tip. Slack 100 and
slack 0 are not separable from each other anywhere in either session, so the
tolerance is doing less than the ordering is.

Two places do not follow. At 1000 ns the arms are intermingled — the age and
peek arms reach 0.5639–0.5940 against 0.5454–0.5769 for `k2` and 0.5470–0.5592
for the tip — so nothing is settled there in one run. At 30000 ns `peek100` is
the only arm of the whole series to sit below the reference, 0.6337 and 0.6573
against 0.6719 and 0.6655 for `934bd1c` and 0.7162 and 0.6871 for the tip.

Throughput keeps most of the own-queue gain. `fillrandom` reads 246.502 and
244.936 Kops/s for `age100`, 245.468 and 248.697 for `age0`, 250.956 and
246.056 for `peek100` and 245.789 and 245.116 for `peek0`, against 252.781 and
256.827 for `k2` in the first session, 252.301 and 261.140 in the second, and
225.789 and 224.671 and then 224.397 and 220.059 for the tip. `readrandom` sits between 0.937x and
1.013x of `934bd1c` on every arm of both sessions. Streamcluster stays inside
the 89–102 s band that the shard family has occupied since the tip, with the
ordering inside that band changing between the two sessions, so it separates
nothing.

The cost is in the dispatch program. The commit messages report `f1a5cf3` at
15419 instructions with enqueue at 428, and `5b25e15` at 15480 with enqueue at
389 — against 256 and 415 at the tip and 8159 at `bf7080f` — and `5b25e15`
records that the lockless queue peek resolved on every load, so the fallback
iterator was never needed on this kernel. The ordering it adds is structural
rather than heuristic: with the bank held as priority queues, the head is the
oldest request by construction and the flush's placement flags stop mattering.

Adoption waits on a multi-repeat confirm of `5b25e15` at slack 100 across the
workloads that decided the series, since everything above rests on one run per
cell. That confirmation is still owed: the branch was merged before it, on the
strength of these screens, and the Decision section records what was shipped.

## Streamcluster gap diagnosis

After the merge, streamcluster on `4b3b0a8` runs where the shard family has
always run, and a set of sessions on 2026-09-08 asked why it does not run
faster. Every arm is `4b3b0a8` unless a commit is named, and every session in
this part is **one run per cell** unless it says otherwise.

### The custody limit sweeps — `custody-ms` and `custody-ms-long`

Both directions of `ACCORDIN_CV_CUSTODY_MS` around the 20 ms default, on
streamcluster.

| session | arm | env | backend | stream s | rel | parked | expired | expired share |
|---|---|---|---|---:|---:|---:|---:|---:|
| `custody-ms` | tip | — | mcs_accordin | 107.827 | 1.000x | 8430666 | 57668 | 0.68 % |
| `custody-ms` | ms5 | `CUSTODY_MS=5` | mcs_accordin | 129.789 | 1.204x | 8432236 | 888719 | 10.5 % |
| `custody-ms` | ms2 | `CUSTODY_MS=2` | mcs_accordin | 137.650 | 1.277x | 8413941 | 1042072 | 12.4 % |
| `custody-ms` | tip | — | mcs_tas_accordin | 109.429 | 1.000x | 8431734 | 63562 | 0.75 % |
| `custody-ms` | ms5 | `CUSTODY_MS=5` | mcs_tas_accordin | 128.363 | 1.173x | 8433164 | 874079 | 10.4 % |
| `custody-ms` | ms2 | `CUSTODY_MS=2` | mcs_tas_accordin | 131.318 | 1.200x | 8414809 | 980950 | 11.7 % |
| `custody-ms-long` | tip | — | mcs_accordin | 90.614 | 1.000x | 8428820 | 45341 | 0.54 % |
| `custody-ms-long` | ms50 | `CUSTODY_MS=50` | mcs_accordin | 113.480 | 1.252x | 8432652 | 748 | 0.009 % |
| `custody-ms-long` | ms100 | `CUSTODY_MS=100` | mcs_accordin | 109.199 | 1.205x | 8432754 | 695 | 0.008 % |
| `custody-ms-long` | tip | — | mcs_tas_accordin | 103.473 | 1.000x | 8428368 | 59986 | 0.71 % |
| `custody-ms-long` | ms50 | `CUSTODY_MS=50` | mcs_tas_accordin | 108.769 | 1.051x | 8432194 | 1202 | 0.014 % |
| `custody-ms-long` | ms100 | `CUSTODY_MS=100` | mcs_tas_accordin | 123.032 | 1.189x | 8433395 | 1195 | 0.014 % |

`fillrandom` in the same `custody-ms` session: 255.739 Kops/s at the tip
against 261.304 at 5 ms and 239.128 at 2 ms on `mcs_accordin`, and 256.687
against 266.348 and 235.904 on `mcs_tas_accordin`, with expiries rising from
944 to 17899 and from 1544 to 19522 at the shortest limit.

Both directions are slower than the default, and that is the finding. Cutting
the limit to 5 or 2 ms pushes 10–12 % of parks past it, and those waits leave
custody through the timer and fall back to the futex path; streamcluster then
runs 17–28 % slower. Raising it to 50 or 100 ms removes the expiries almost
entirely — 0.01 % of parks — and streamcluster is 5–25 % slower again. A wait
that expires and goes back to the futex is therefore served *faster* than one
the scheduler releases into the admission bank, and holding waits longer to
release more of them the scheduler's way loses time rather than saving it.

### Where the expiries come from — `wait-instrument` and `wait-diag`

`wait-instrument` runs one arm of branch `cv-wait-instrument` (`33457f0`),
which splits the expiry counter by whether the waiter had been notified.

| session | commit | backend | stream s | parked | expired | expired notified | expired unnotified |
|---|---|---|---:|---:|---:|---:|---:|
| `wait-instrument` | `33457f0` | mcs_accordin | 104.792 | 8428304 | 55520 | 22800 | 32720 |
| `wait-instrument` | `33457f0` | mcs_tas_accordin | 95.715 | 8427908 | 50504 | 21505 | 28999 |

So of 55520 expiries, 22800 belonged to waits that had been notified and were
still sitting in custody when the limit ran out — the release the scheduler
owed them never arrived. The `wait-diag` sessions then asked when those
notifications happened. The notifier-time probe (`8490b3a`) stamps each
notification against the waiter's custody deadline:

| counter | value |
|---|---:|
| notifications arriving while the wait was still inside its limit | 20 |
| notifications arriving after the limit had already passed | 27048 |
| notifications seen with no flush in flight | 7590 |
| notifications seen with one flush in flight | 19478 |

99.93 % of the notifications behind a notified expiry arrive after the wait's
limit has already gone by, so they are not releases the scheduler dropped:
the waiter had already timed out and the notification simply came later. The
genuinely missed releases are the first row — 20 in that run, and 134 and 159
in the two runs of `wait-fix/stream-v2` that carry the same probe — against
8.4 M parks. Nothing worth chasing is hiding in the expiry counter.

The same sessions priced the flush itself. A pass costs 541 µs
(`wait-fix/probe`, 23977434528 ns over 44296 passes) and covers about 190
parked waits (8429877 parks over 44296 calls), while that run parks 8429877
waits in 101.133 s, so roughly 45 more waits arrive while a pass is in flight;
the `wait-diag/stream3` run puts the same figure at 51 (603.9 µs a pass,
83804 parks a second). Of the flush calls, 27697 of 71946 and 29609 of 73899
in `wait-diag/stream` lose the claim to another thread — about 40 % — and the
walk over the bank refuses or fails to read almost nothing (`unreadable` 0 in
every run, `refused` at most 3). The flush is not failing; it is simply always
behind.

### Never skipping the flush — `wait-fix/stream-clean` and `wait-fix/leveldb-clean`

`94d830a` makes a thread that loses the claim wait for the pass instead of
returning, so no notification goes unflushed.

| session | arm | commit | backend | value | rel | expired | flush calls |
|---|---|---|---|---:|---:|---:|---:|
| stream | base | `33457f0` | mcs_accordin | 102.350 s | 1.000x | 52650 | 50671 |
| stream | clean | `94d830a` | mcs_accordin | 111.638 s | 1.091x | 72179 | 44297 |
| stream | base | `33457f0` | mcs_tas_accordin | 99.006 s | 1.000x | 54259 | 50145 |
| stream | clean | `94d830a` | mcs_tas_accordin | 105.476 s | 1.065x | 70880 | 44308 |
| fillrandom | base | `33457f0` | mcs_accordin | 245.586 Kops/s | 1.000x | 377 | 66076 |
| fillrandom | clean | `94d830a` | mcs_accordin | 261.113 Kops/s | 1.063x | 580 | 47910 |
| fillrandom | base | `33457f0` | mcs_tas_accordin | 239.326 Kops/s | 1.000x | 685 | 65319 |
| fillrandom | clean | `94d830a` | mcs_tas_accordin | 253.059 Kops/s | 1.057x | 1812 | 46174 |
| readrandom | base | `33457f0` | mcs_accordin | 1329.618 Kops/s | 1.000x | 1 | 2 |
| readrandom | clean | `94d830a` | mcs_accordin | 1304.677 Kops/s | 0.981x | 1 | 2 |
| readrandom | base | `33457f0` | mcs_tas_accordin | 1060.648 Kops/s | 1.000x | 1 | 2 |
| readrandom | clean | `94d830a` | mcs_tas_accordin | 1094.198 Kops/s | 1.032x | 1 | 2 |

Waiting for the pass makes streamcluster 9.1 % and 6.5 % slower and raises
expiries by 37 % and 31 %, because a thread that waits is a thread not running
the workload, and the waits it saves expire anyway. `fillrandom` gains about
6 % and `readrandom` is flat. The change is not adopted.

### The progress backstop — `backstop`, `backstop2`, `backstop-rr`

Three commits on `cv_owner_fair` wake an idle CPU for an admission queue whose
head has stopped moving: `455edb9` judges a head by its age, `515813e` limits
the rule to waiters the flush filed and arms it at 300 µs, and `c211073` drops
the write from the dispatch path, defaults to 1 ms and removes the backoff.
The sessions measured the first two.

streamcluster:

| session | arm | commit | env | backend | stream s | rel |
|---|---|---|---|---|---:|---:|
| `backstop` | tip | `4b3b0a8` | — | mcs_accordin | 96.049 | 1.000x |
| `backstop` | bs300 | `455edb9` | — | mcs_accordin | 92.369 | 0.962x |
| `backstop` | bs1000 | `455edb9` | `PROGRESS_US=1000` | mcs_accordin | 95.039 | 0.989x |
| `backstop` | tip | `4b3b0a8` | — | mcs_tas_accordin | 101.839 | 1.000x |
| `backstop` | bs300 | `455edb9` | — | mcs_tas_accordin | 81.970 | 0.805x |
| `backstop` | bs1000 | `455edb9` | `PROGRESS_US=1000` | mcs_tas_accordin | 86.283 | 0.847x |
| `backstop2` | tip | `4b3b0a8` | — | mcs_accordin | 99.955 | 1.000x |
| `backstop2` | bs | `515813e` | — | mcs_accordin | 83.875 | 0.839x |
| `backstop2` | bsoff | `515813e` | `PROGRESS_US=0` | mcs_accordin | 99.489 | 0.995x |
| `backstop2` | tip | `4b3b0a8` | — | mcs_tas_accordin | 101.974 | 1.000x |
| `backstop2` | bs | `515813e` | — | mcs_tas_accordin | 95.321 | 0.935x |
| `backstop2` | bsoff | `515813e` | `PROGRESS_US=0` | mcs_tas_accordin | 95.396 | 0.935x |

LevelDB:

| session | arm | commit | env | backend | fill Kops/s | rel | read Kops/s | rel |
|---|---|---|---|---|---:|---:|---:|---:|
| `backstop` | tip | `4b3b0a8` | — | mcs_accordin | 233.366 | 1.000x | 1258.986 | 1.000x |
| `backstop` | bs300 | `455edb9` | — | mcs_accordin | 200.755 | 0.860x | 1097.428 | 0.872x |
| `backstop` | bs1000 | `455edb9` | `PROGRESS_US=1000` | mcs_accordin | 236.666 | 1.014x | 1121.225 | 0.891x |
| `backstop` | tip | `4b3b0a8` | — | mcs_tas_accordin | 241.470 | 1.000x | 1048.106 | 1.000x |
| `backstop` | bs300 | `455edb9` | — | mcs_tas_accordin | 222.495 | 0.921x | 1011.846 | 0.965x |
| `backstop` | bs1000 | `455edb9` | `PROGRESS_US=1000` | mcs_tas_accordin | 237.975 | 0.986x | 1028.258 | 0.981x |
| `backstop2` | tip | `4b3b0a8` | — | mcs_accordin | 240.508 | 1.000x | 1300.597 | 1.000x |
| `backstop2` | bs | `515813e` | — | mcs_accordin | 224.531 | 0.934x | 1046.146 | 0.804x |
| `backstop2` | bsoff | `515813e` | `PROGRESS_US=0` | mcs_accordin | 246.984 | 1.027x | 1050.316 | 0.808x |
| `backstop2` | tip | `4b3b0a8` | — | mcs_tas_accordin | 237.161 | 1.000x | 1059.019 | 1.000x |
| `backstop2` | bs | `515813e` | — | mcs_tas_accordin | 214.346 | 0.904x | 977.621 | 0.923x |
| `backstop2` | bsoff | `515813e` | `PROGRESS_US=0` | mcs_tas_accordin | 248.688 | 1.049x | 1016.801 | 0.960x |

`backstop-rr`, three repeats, `readrandom` with the timer unarmed on both
backstop commits:

| arm | commit | env | backend | read Kops/s | CV% | rel |
|---|---|---|---|---:|---:|---:|
| tip | `4b3b0a8` | — | mcs_accordin | 1268.365 | 0.98 | 1.000x |
| bs1off | `455edb9` | `PROGRESS_US=0` | mcs_accordin | 1118.186 | 3.55 | 0.882x |
| bs2off | `515813e` | `PROGRESS_US=0` | mcs_accordin | 1097.032 | 1.29 | 0.865x |
| tip | `4b3b0a8` | — | mcs_tas_accordin | 1010.152 | 10.06 | 1.000x |
| bs1off | `455edb9` | `PROGRESS_US=0` | mcs_tas_accordin | 995.534 | 2.94 | 0.986x |
| bs2off | `515813e` | `PROGRESS_US=0` | mcs_tas_accordin | 997.391 | 1.55 | 0.987x |

Per-thread fairness at 192 threads (`backstop/exp7`) moves very little: the
factor reads 0.5206 / 0.5222 / 0.5234 at 100 ns on `mcs_accordin_direct` and
0.6572 / 0.6575 / 0.6580 at 30000 ns for tip, `bs300` and `bs1000`.

The backstop does what it was written to do on streamcluster — 4 % to 20 %
faster, the largest gains on `mcs_tas_accordin` — and costs `fillrandom` 7 % to
14 % at the 300 µs setting, which the 1 ms setting mostly recovers. What stops
it is `readrandom` on `mcs_accordin`: 0.872x, 0.804x and, over three repeats
with the timer switched off entirely, 0.882x and 0.865x at CV under 3.6 %. An
arm whose timer never fires cannot be losing time to the timer, so the loss
belongs to the commit's presence rather than to its behaviour, which is the
same shape as the `readrandom` question below.

### The `readrandom` code-volume effect — `layout-rr` through `layout-rr5`

Every arm below is `readrandom` at 192 threads, three repeats, `rel` against
the tip of its own session. The question is why a commit that does nothing at
run time still costs `readrandom` on `mcs_accordin`.

| session | arm | commit | what the arm carries | MCS rel | MCS-TAS rel |
|---|---|---|---|---:|---:|
| `layout-rr` | bs1off | `455edb9` | backstop, timer unarmed | 0.861x | 0.990x |
| `layout-rr` | L1 | `d426aa8` | admission word off its neighbours' lines | 0.882x | 0.968x |
| `layout-rr` | L2 | `758ca2d` | a cache line per admission slot | 0.864x | 0.966x |
| `layout-rr` | L3 | `39d072b` | backstop globals moved past the grant order's | 0.767x | 0.964x |
| `layout-rr2` | G | `8d4d96b` | the backstop's counters, no backstop | 1.028x | 1.036x |
| `layout-rr2` | P | `6e09b70` | the callback loaded, never allowed to run | 0.874x | 0.957x |
| `layout-rr2` | R | `02bbeff` | the BPF object with no knob to arm it | 0.857x | 0.932x |
| `layout-rr3` | Pmap | `1e0392a` | a second timer held, neither end used | 1.011x | 0.977x |
| `layout-rr3` | Pcode | `a38c016` | the walk loaded with no timer to reach it | 0.905x | 0.942x |
| `layout-rr4` | D1 | `425b757` | an unreached bank walk behind setup | 0.838x | 0.906x |
| `layout-rr4` | D2 | `2774b32` | an unreached bank walk behind the transfer pass | 0.849x | 0.913x |
| `layout-rr4` | D3 | `5b44f32` | an unreached bank walk behind the dump | 0.875x | 0.896x |
| `layout-rr5` | U1 | `917b9fa` | read-only ballast the size of a larger object | 1.024x | 0.985x |
| `layout-rr5` | U2 | `a7ae4f3` | the admission word on its own line, TLS aligned | 0.996x | 0.981x |

The reference arm for the last three sessions, `Pcode`, repeats at 0.905x,
0.848x, 0.887x, and again at 0.875x under `bpf_stats` (`layout-perf`) and
0.849x with perf counters attached (`layout-perfk`), so the effect survives
instrumentation.

Read together: data layout does not explain it (`L1`, `L2`, `L3` and `U2`
change placement and recover nothing), userspace does not (`G` publishes the
globals alone and is neutral at 1.028x, `U1` carries read-only ballast and is
neutral at 1.024x, and the TLS block is identical across arms), and the host
program does not (`R` loads the object with no knob at all and still loses
14 %). What moves `readrandom` is BPF text loaded into the kernel, whether or
not anything can reach it: an extra map with no user is free (`Pmap`, 1.011x),
dead instructions are not (`Pcode`, `D1`, `D2`, `D3`, all 0.838x–0.905x).
Under instrumentation the loaded programs cost the same per invocation and are
simply invoked less often at the same cycle count with a lower IPC, and the
instruction-fetch counters — iTLB and icache miss rates — do not move. A
metastable idle-and-wake regime is the open hypothesis; it is being probed and
nothing here settles it. `readrandom` on `mcs_accordin` loses 10–16 % from the
volume of scheduler text on the machine, and the series has no mechanism for
that yet.

The scratch branches carrying these arms, each one commit on top of the tip:

| branch | commit | what it changes |
|---|---|---|
| `readrandom-layout` | `d426aa8` | keeps the admission word off its neighbours' cache lines |
| `layout-slotline` | `758ca2d` | gives every admission slot a cache line of its own |
| `layout-place` | `39d072b` | moves the backstop's globals past the grant order's own |
| `prog-globals` | `8d4d96b` | publishes the backstop's counters without the backstop |
| `prog-callback` | `6e09b70` | loads the backstop's callback without letting it run |
| `prog-noknob` | `02bbeff` | loads the backstop with no runtime knob to arm it |
| `prog-maponly` | `1e0392a` | holds a second timer for the backstop and uses neither end |
| `prog-codeonly` | `a38c016` | loads the backstop walk with no timer to reach it |
| `pad-init` | `425b757` | loads an unreached bank walk behind the scheduler's setup |
| `pad-flush` | `2774b32` | loads an unreached bank walk behind the transfer pass |
| `pad-dump` | `5b44f32` | loads an unreached bank walk behind the scheduler's dump |
| `rodata-ballast` | `917b9fa` | carries read-only ballast the size of a larger object |
| `tls-align` | `a7ae4f3` | keeps the admission word on a cache line of its own |

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
- **The `percpu`, `bounded`, `age` and `peek` screens are one run per cell.**
  All four were run with reduced repeats to answer a structural question
  quickly. Their throughput figures are indicative only; only fairness
  orderings that are far larger than any spread the multi-repeat sessions show
  on that metric, and that repeat across two of these screens, are treated as
  settled. Individual cells in them go visibly wrong — the tip at half
  throughput at one `age` point, its `readrandom` at 0.896x in `peek` — without
  invalidating the session around them. The same holds for the streamcluster
  diagnosis sessions, which are single-repeat except `backstop-rr` and the
  `layout-rr` series.
