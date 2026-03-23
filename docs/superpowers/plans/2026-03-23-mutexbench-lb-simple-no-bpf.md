# Mutexbench Lb Simple No BPF Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a first-class `lb_simple_no_bpf` lock item to `bench/mutexbench/scripts/sweep_mutex_throughput_multi_lock.sh` so mutexbench sweeps can run lb_simple with `LB_SIMPLE_DISABLE_BPF=1` without wrapping the command manually.

**Architecture:** Extend the multi-lock parser with one additional lb_simple-derived mode that keeps the existing `lb_simple` preload path resolution and sched_ext handling, but prefixes the inner sweep command with `env LB_SIMPLE_DISABLE_BPF=1`. Cover the new behavior with a dry-run shell test that asserts the generated command and output directory naming, then document the new lock item in the mutexbench README.

**Tech Stack:** Bash, existing mutexbench shell test pattern, README documentation

---

### Task 1: Add regression coverage for `lb_simple_no_bpf`

**Files:**
- Modify: `bench/mutexbench/scripts/test_sweep_mutex_throughput_multi_lock_flexguard.sh`
- Test: `bench/mutexbench/scripts/test_sweep_mutex_throughput_multi_lock_flexguard.sh`

- [ ] **Step 1: Write the failing test**

Add a dry-run case that invokes:

```bash
"${MULTI_LOCK_SCRIPT}" \
  --locks lb_simple_no_bpf \
  --sweep-script "${fake_sweep_script}" \
  --sudo-mode none \
  --threads 1 \
  --critical-ns 10 \
  --outside-ns 10 \
  --duration-ms 1 \
  --warmup-duration-ms 1 \
  --repeats 1 \
  --output-root "${tmpdir}/results" \
  --dry-run
```

Expected assertions:

```bash
grep -F "lock=lb_simple_no_bpf" "${output_log}"
grep -F "LB_SIMPLE_DISABLE_BPF=1" "${output_log}"
grep -F "--bench-ld-preload" "${output_log}"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `bash bench/mutexbench/scripts/test_sweep_mutex_throughput_multi_lock_flexguard.sh`
Expected: FAIL because `lb_simple_no_bpf` is not recognized yet.

### Task 2: Implement `lb_simple_no_bpf` in the multi-lock sweep script

**Files:**
- Modify: `bench/mutexbench/scripts/sweep_mutex_throughput_multi_lock.sh`
- Test: `bench/mutexbench/scripts/test_sweep_mutex_throughput_multi_lock_flexguard.sh`

- [ ] **Step 3: Write minimal implementation**

Update parsing and command construction so that:

```bash
--locks lb_simple_no_bpf
```

behaves like:

```bash
env LB_SIMPLE_DISABLE_BPF=1 \
  scripts/sweep_mutex_throughput.sh \
  --bench-ld-preload <liblb_simple.so> \
  --lock-kind mutex
```

Implementation constraints:

- Preserve existing `lb_simple` behavior.
- Reuse lb_simple library resolution and sched_ext conflict handling.
- Keep output directory name as `lb_simple_no_bpf`.
- Keep `sudo-mode auto` treating this mode like `lb_simple`.

- [ ] **Step 4: Run test to verify it passes**

Run: `bash bench/mutexbench/scripts/test_sweep_mutex_throughput_multi_lock_flexguard.sh`
Expected: PASS.

### Task 3: Document the new lock item

**Files:**
- Modify: `bench/mutexbench/README.md`

- [ ] **Step 5: Update README**

Document that `--locks` now accepts both:

```text
lb_simple
lb_simple_no_bpf
```

and explain that `lb_simple_no_bpf` runs with `LB_SIMPLE_DISABLE_BPF=1`.

- [ ] **Step 6: Re-run focused verification**

Run:

```bash
bash bench/mutexbench/scripts/test_sweep_mutex_throughput_multi_lock_flexguard.sh
```

Expected: PASS.
