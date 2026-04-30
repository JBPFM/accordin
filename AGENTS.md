# Repository Instructions

## Working Style
- Explain the approach before implementation.
- If a request is ambiguous, high risk, or has broad impact, clarify and wait for approval before editing code.
- Plans describe the approach only; do not include code.
- Follow spec-driven coding rather than exploratory changes.

## Coding Rules
- Use English in code and comments.
- Do not rely on line numbers when writing specs or implementation notes.
- Do not write process notes in comments.
- Prefer conceptual descriptions over file-path-and-line references in specs.

## Task Scope
- Split work into low-coupling subtasks that can be verified independently.
- Keep changes scoped to the requested behavior.
- Do not revert unrelated user changes.

## Quality Bar
- Keep the early-stage quality bar minimal but strict: runnable, verifiable, and reversible.
- Prioritize verification for critical paths and high-risk changes.
- For bug fixes, reproduce the issue before fixing when practical, then verify the fix.

## Performance Experiments
- All performance experiments and benchmark runs must hold the repository performance lock for the full duration of the run.
- Use this lock file: `/tmp/lb_simple_performance_experiment.lock`.
- The lock must cover every script under `experiments/`, including current and future experiment runners.
- The lock must also cover commands that run benchmarks, load eBPF or `sched_ext` schedulers, use timeslice extension helpers, or run `LD_PRELOAD` benchmark libraries.
- Prefer a blocking exclusive lock so concurrent agents wait instead of running conflicting experiments:

```bash
flock -x /tmp/lb_simple_performance_experiment.lock -c 'sudo -E python3 experiments/run_experiment_two.py --build-missing'
```

- Do not run `experiments/*.py` directly. Wrap the script invocation with the lock:

```bash
flock -x /tmp/lb_simple_performance_experiment.lock -c 'python3 experiments/run_experiment_one.py'
```

- For multi-command experiments, wrap the whole sequence in one locked shell:

```bash
flock -x /tmp/lb_simple_performance_experiment.lock -c '
  sudo -E python3 experiments/run_experiment_two.py --build-missing --repeats 1
'
```

- If a benchmark cannot be wrapped directly with `flock`, acquire the same lock in the parent shell before starting any child benchmark process.
- Do not start a second benchmark while another agent holds the lock.

## Collaboration
- When corrected, identify the cause and adjust the workflow.
- Keep implementation and review separate when the risk justifies it.

## Avoid
- Avoid development-progress terms in code comments, commit messages, and PR bodies.
- Avoid AI tool names in code comments, commit messages, and PR bodies.
