# Experiment 9 Buckets Runner Design

## Goal

Add a root-level experiment runner for the existing FlexGuard `buckets` hash-table benchmark. The runner should live under `experiments/`, match the CSV-oriented output style of the other `run_experiment_*.py` scripts, and make bucket benchmark runs reproducible without using the older FlexGuard suite wrapper.

## Scope

In scope:

- Add `experiments/run_experiment_nine.py`.
- Run `bench/flexguard/build/buckets_<lock>` for selected locks, thread counts, repeats, and one configured bucket count.
- Produce `raw.csv`, `summary.csv`, `settings.json`, `commands.json`, per-command logs, and optional PNG plots.
- Support `--plot-only`, `--skip-plots`, `--dry-run`, `--resume`, `--force`, `--build-missing`, lock selection, and benchmark parameter overrides.
- Reuse `experiments/experiment_defaults.py` for lock profiles, normalization, labels, thread defaults, and single-oversubscribed limits.

Out of scope:

- Changing `bench/flexguard/bmarks/buckets.c` behavior.
- Reworking the older FlexGuard Python suite under `bench/flexguard/scripts/suite`.
- Mixing bucket benchmark results into Experiment 1 mutexbench outputs.
- Adding Accordin direct-adapter support for the bucket benchmark unless the needed executable path already exists as `buckets_<lock>`.

## Runner Interface

The new runner will default to:

- output root: `experiments/results/experiment9_buckets_<timestamp>`
- lock profile: `minimal`
- threads: `experiment_defaults.DEFAULT_THREADS`
- duration: `10000` ms
- buckets: `100`
- max value: `100000`
- offset changes: `40`
- non-critical cycles: `0`
- pin threads: disabled
- repeats: `experiment_defaults.DEFAULT_REPEATS`

Primary CLI options:

- `--output-root PATH`
- `--plot-only PATH`
- `--skip-plots`
- `--dry-run`
- `--force`
- `--resume`
- `--build-missing`
- `--lock-profile {minimal,full}`
- `--locks a,b,c`
- `--threads 1,2,4,...`
- `--duration-ms N`
- `--buckets N`
- `--max-value N`
- `--offset-changes N`
- `--non-critical-cycles N`
- `--pin-threads`
- `--repeats N`
- `--command-timeout-seconds N`

## Lock Handling

The runner will normalize lock names with `experiment_defaults.normalize_locks()`. It will treat a selected lock as runnable only if the corresponding executable exists at:

`bench/flexguard/build/buckets_<lock>`

If one or more executables are missing and `--build-missing` is not provided, the runner will fail before starting any benchmark rows and report the missing paths. If `--build-missing` is provided, it will run:

`bash bench/flexguard/scripts/make_all.sh`

After the build, it will recheck the required executables and fail if any selected lock remains unavailable.

Single-oversubscribed locks will use `experiment_defaults.runnable_threads_for_lock()` so they follow the same thread limiting rules as the other experiments.

## Data Model

`raw.csv` will contain one row per lock, thread count, and repeat:

- `lock`
- `lock_label`
- `threads`
- `duration_ms`
- `buckets`
- `max_value`
- `offset_changes`
- `non_critical_cycles`
- `pin_threads`
- `repeat`
- `throughput_cs_per_sec`
- `mean_thread_throughput_cs_per_sec`
- `min_thread_throughput_cs_per_sec`
- `max_thread_throughput_cs_per_sec`
- `thread_iterations_total`
- `pauses`
- `wall_seconds`
- `command_log`

`summary.csv` will group rows by lock, thread count, and benchmark parameters, then average numeric fields across repeats. It will include a `runs` count.

`settings.json` will capture selected locks, lock profile, runnable threads by lock, machine profile, benchmark parameters, repeats, build behavior, timeout, and FlexGuard paths.

`commands.json` will record command text, cwd, start/end timestamps, return code, wall time, and log path for build and benchmark commands.

## Benchmark Output Parsing

The runner will parse the existing buckets output:

- global throughput from `#Throughput: <value> CS/s`
- per-thread throughput and iterations from `#Local result for Thread <id>: <value> CS/s (<iterations> iterations)`
- pause count from `Pauses: <value>` when present

If a benchmark command exits non-zero or required throughput is missing, the row is considered failed. The runner should write the command log and record the failed command in `commands.json`; failed rows are not included in `raw.csv`.

## Plotting

When matplotlib is available and `--skip-plots` is not set, generate:

- `plots/throughput_vs_threads.png`

Plots are secondary. `raw.csv` and `summary.csv` remain the source of truth.

## Error Handling

- Reject invalid numeric parameters at argument parsing time.
- Refuse to overwrite existing result CSV/settings files unless `--force` is set.
- With `--resume`, skip complete raw rows already present for the exact target tuple.
- Fail early on missing executables unless `--build-missing` is set.
- Preserve command logs for failures.
- Do not modify unrelated result roots or existing experiment scripts.

## Validation

Implementation should be validated with:

1. `python3 -m py_compile experiments/run_experiment_nine.py`
2. `python3 experiments/run_experiment_nine.py --dry-run --locks mutex --threads 1 --repeats 1`
3. A short smoke run if the needed executable exists or builds successfully:
   `python3 experiments/run_experiment_nine.py --locks mutex --threads 1 --duration-ms 100 --offset-changes 1 --repeats 1 --skip-plots`

If matplotlib is unavailable, plot generation can be left unverified as long as CSV generation is verified.
