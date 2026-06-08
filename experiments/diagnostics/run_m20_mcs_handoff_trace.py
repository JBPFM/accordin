#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import os
import re
import signal
import statistics
import subprocess
import sys
import time
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
DIAG_ROOT = REPO_ROOT / "experiments" / "diagnostics"
SRC = DIAG_ROOT / "mcs_handoff_trace.cpp"
EVENT_BT_TEMPLATE = DIAG_ROOT / "mcs_handoff_trace.bt.in"
AGGREGATE_BT_TEMPLATE = DIAG_ROOT / "mcs_handoff_aggregate.bt.in"
BUILD_DIR = REPO_ROOT / "target" / "diagnostics"
BIN = BUILD_DIR / "mcs_handoff_trace"
RESULTS_ROOT = REPO_ROOT / "experiments" / "results"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run the MCS handoff/scheduler mismatch diagnostic on m20/W40."
    )
    parser.add_argument("--threads", default="32,40,64,96,192")
    parser.add_argument("--duration-ms", type=int, default=1200)
    parser.add_argument("--warmup-ms", type=int, default=300)
    parser.add_argument("--startup-delay-ms", type=int, default=2500)
    parser.add_argument("--critical-ns", type=int, default=300)
    parser.add_argument("--outside-ns", type=int, default=3000)
    parser.add_argument("--trace-stride", type=int, default=16)
    parser.add_argument(
        "--trace-mode",
        choices=("aggregate", "events"),
        default="aggregate",
        help="aggregate keeps counters/histograms in BPF; events writes one CSV row per handoff.",
    )
    parser.add_argument("--bpftrace-bin", default="/usr/bin/bpftrace")
    parser.add_argument(
        "--output-root",
        type=Path,
        default=None,
        help="Default: experiments/results/mcs_handoff_trace_m20_<timestamp>",
    )
    parser.add_argument("--skip-build", action="store_true")
    return parser.parse_args()


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    print("+", " ".join(cmd), flush=True)
    return subprocess.run(cmd, check=True, text=True, **kwargs)


def build() -> None:
    BUILD_DIR.mkdir(parents=True, exist_ok=True)
    run(
        [
            "g++",
            "-O2",
            "-g",
            "-std=c++20",
            "-pthread",
            "-fno-omit-frame-pointer",
            str(SRC),
            "-o",
            str(BIN),
        ]
    )
    run(["nm", "-C", str(BIN)], stdout=subprocess.DEVNULL)


def render_bpftrace(pid: int, out_dir: Path, trace_mode: str) -> Path:
    template = AGGREGATE_BT_TEMPLATE if trace_mode == "aggregate" else EVENT_BT_TEMPLATE
    script = template.read_text(encoding="utf-8")
    script = script.replace("__BIN__", str(BIN)).replace("__PID__", str(pid))
    path = out_dir / "mcs_handoff_trace.bt"
    path.write_text(script, encoding="utf-8")
    return path


def run_one(args: argparse.Namespace, threads: int, out_root: Path) -> dict[str, str]:
    out_dir = out_root / f"threads_{threads}"
    out_dir.mkdir(parents=True, exist_ok=True)
    bench_stdout = out_dir / "bench_stdout.txt"
    bench_stderr = out_dir / "bench_stderr.txt"
    trace_output = (
        out_dir / "handoff_aggregate.txt"
        if args.trace_mode == "aggregate"
        else out_dir / "handoff_trace.csv"
    )
    trace_stderr = out_dir / "bpftrace_stderr.txt"

    bench_cmd = [
        str(BIN),
        "--threads",
        str(threads),
        "--duration-ms",
        str(args.duration_ms),
        "--warmup-ms",
        str(args.warmup_ms),
        "--startup-delay-ms",
        str(args.startup_delay_ms),
        "--critical-ns",
        str(args.critical_ns),
        "--outside-ns",
        str(args.outside_ns),
        "--trace-stride",
        str(args.trace_stride),
    ]

    with bench_stdout.open("w", encoding="utf-8") as bench_out, bench_stderr.open(
        "w", encoding="utf-8"
    ) as bench_err:
        bench = subprocess.Popen(
            bench_cmd,
            stdout=bench_out,
            stderr=bench_err,
            text=True,
        )

    script = render_bpftrace(bench.pid, out_dir, args.trace_mode)
    with trace_output.open("w", encoding="utf-8") as trace_out, trace_stderr.open(
        "w", encoding="utf-8"
    ) as trace_err:
        if args.trace_mode == "events":
            trace_out.write(
                "event,sample_id,release_ns,owner_tid,successor_tid,acquire_tid,"
                "successor_seq,successor_oncpu_at_release,"
                "successor_dispatch_delay_ns,handoff_gap_ns,"
                "later_waiter_switches\n"
            )
            trace_out.flush()
        tracer = subprocess.Popen(
            ["sudo", "-n", args.bpftrace_bin, "-q", str(script)],
            stdout=trace_out,
            stderr=trace_err,
            text=True,
            preexec_fn=os.setsid,
        )

        try:
            rc = bench.wait(timeout=(args.startup_delay_ms + args.warmup_ms + args.duration_ms) / 1000 + 30)
            if rc != 0:
                raise subprocess.CalledProcessError(rc, bench_cmd)
        finally:
            try:
                os.killpg(os.getpgid(tracer.pid), signal.SIGINT)
            except ProcessLookupError:
                pass
            try:
                tracer.wait(timeout=10)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(os.getpgid(tracer.pid), signal.SIGKILL)
                except ProcessLookupError:
                    pass
                tracer.wait(timeout=5)

    if args.trace_mode == "aggregate":
        summary = summarize_aggregate(trace_output)
    else:
        summary = summarize_events(trace_output)
    summary["threads"] = str(threads)
    summary["trace_mode"] = args.trace_mode
    summary["trace_output"] = str(trace_output)
    summary["bench_stdout"] = str(bench_stdout)
    return summary


def percentile(values: list[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = int(round((pct / 100.0) * (len(ordered) - 1)))
    return ordered[max(0, min(index, len(ordered) - 1))]


def weighted_percentile_us(bucket_counts: dict[int, int], pct: float) -> float:
    total = sum(bucket_counts.values())
    if total == 0:
        return 0.0
    target = max(1, int(round((pct / 100.0) * total)))
    seen = 0
    for bucket_us, count in sorted(bucket_counts.items()):
        seen += count
        if seen >= target:
            return float(bucket_us)
    return float(max(bucket_counts))


def parse_bucket_maps(path: Path) -> tuple[dict[str, int], dict[str, dict[int, int]]]:
    summary: dict[str, int] = {}
    buckets: dict[str, dict[int, int]] = {
        "handoff_gap_us": {},
        "dispatch_us": {},
        "offcpu_dispatch_us": {},
    }
    map_line = re.compile(
        r"^@(handoff_gap_us|dispatch_us|offcpu_dispatch_us)\[(\d+)\]:\s+(\d+)\s*$"
    )
    if not path.is_file():
        return summary, buckets
    with path.open(encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if line.startswith("summary,"):
                _, key, value = line.split(",", 2)
                summary[key] = int(value)
                continue
            match = map_line.match(line)
            if match:
                name, bucket, count = match.groups()
                buckets[name][int(bucket)] = int(count)
    return summary, buckets


def summarize_aggregate(path: Path) -> dict[str, str]:
    counts, buckets = parse_bucket_maps(path)
    sampled = counts.get("sampled_handoffs", 0)
    offcpu = counts.get("successor_offcpu_count", 0)
    later = counts.get("later_waiter_handoff_count", 0)
    later_sum = counts.get("later_waiter_switch_sum", 0)

    median_gap_ns = weighted_percentile_us(buckets["handoff_gap_us"], 50) * 1000
    p95_gap_ns = weighted_percentile_us(buckets["handoff_gap_us"], 95) * 1000
    median_dispatch_ns = weighted_percentile_us(buckets["dispatch_us"], 50) * 1000
    p95_dispatch_ns = weighted_percentile_us(buckets["dispatch_us"], 95) * 1000
    median_offcpu_dispatch_ns = (
        weighted_percentile_us(buckets["offcpu_dispatch_us"], 50) * 1000
    )
    p95_offcpu_dispatch_ns = (
        weighted_percentile_us(buckets["offcpu_dispatch_us"], 95) * 1000
    )

    return {
        "sampled_handoffs": str(sampled),
        "successor_offcpu_fraction": f"{offcpu / sampled:.6f}" if sampled else "0",
        "later_waiter_switch_fraction": f"{later / sampled:.6f}" if sampled else "0",
        "median_handoff_gap_ns": f"{median_gap_ns:.2f}",
        "p95_handoff_gap_ns": f"{p95_gap_ns:.2f}",
        "median_dispatch_delay_ns": f"{median_dispatch_ns:.2f}",
        "p95_dispatch_delay_ns": f"{p95_dispatch_ns:.2f}",
        "median_offcpu_dispatch_delay_ns": f"{median_offcpu_dispatch_ns:.2f}"
        if offcpu
        else "0",
        "p95_offcpu_dispatch_delay_ns": f"{p95_offcpu_dispatch_ns:.2f}"
        if offcpu
        else "0",
        "mean_later_waiter_switches": f"{later_sum / sampled:.4f}"
        if sampled
        else "0",
    }


def summarize_events(path: Path) -> dict[str, str]:
    rows: list[dict[str, str]] = []
    if path.is_file():
        with path.open(newline="", encoding="utf-8") as handle:
            reader = csv.DictReader(handle)
            for row in reader:
                if row.get("event") == "handoff":
                    rows.append(row)
    gaps = [float(row["handoff_gap_ns"]) for row in rows]
    dispatch = [float(row["successor_dispatch_delay_ns"]) for row in rows]
    offcpu_dispatch = [
        float(row["successor_dispatch_delay_ns"])
        for row in rows
        if row["successor_oncpu_at_release"] == "0"
    ]
    later_switches = [float(row["later_waiter_switches"]) for row in rows]
    offcpu_count = sum(1 for row in rows if row["successor_oncpu_at_release"] == "0")
    later_count = sum(1 for row in rows if int(row["later_waiter_switches"]) > 0)
    return {
        "sampled_handoffs": str(len(rows)),
        "successor_offcpu_fraction": f"{offcpu_count / len(rows):.6f}" if rows else "0",
        "later_waiter_switch_fraction": f"{later_count / len(rows):.6f}" if rows else "0",
        "median_handoff_gap_ns": f"{statistics.median(gaps):.2f}" if gaps else "0",
        "p95_handoff_gap_ns": f"{percentile(gaps, 95):.2f}" if gaps else "0",
        "median_dispatch_delay_ns": f"{statistics.median(dispatch):.2f}" if dispatch else "0",
        "p95_dispatch_delay_ns": f"{percentile(dispatch, 95):.2f}" if dispatch else "0",
        "median_offcpu_dispatch_delay_ns": f"{statistics.median(offcpu_dispatch):.2f}"
        if offcpu_dispatch
        else "0",
        "p95_offcpu_dispatch_delay_ns": f"{percentile(offcpu_dispatch, 95):.2f}"
        if offcpu_dispatch
        else "0",
        "mean_later_waiter_switches": f"{statistics.fmean(later_switches):.4f}"
        if later_switches
        else "0",
    }


def main() -> int:
    args = parse_args()
    if not args.skip_build:
        build()
    timestamp = time.strftime("%Y%m%d_%H%M%S")
    out_root = args.output_root or RESULTS_ROOT / f"mcs_handoff_trace_m20_{timestamp}"
    out_root.mkdir(parents=True, exist_ok=True)

    thread_values = [int(item) for item in args.threads.split(",") if item.strip()]
    summaries = [run_one(args, threads, out_root) for threads in thread_values]

    summary_path = out_root / "summary.csv"
    fieldnames = [
        "threads",
        "trace_mode",
        "sampled_handoffs",
        "successor_offcpu_fraction",
        "later_waiter_switch_fraction",
        "median_handoff_gap_ns",
        "p95_handoff_gap_ns",
        "median_dispatch_delay_ns",
        "p95_dispatch_delay_ns",
        "median_offcpu_dispatch_delay_ns",
        "p95_offcpu_dispatch_delay_ns",
        "mean_later_waiter_switches",
        "trace_output",
        "bench_stdout",
    ]
    with summary_path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=fieldnames)
        writer.writeheader()
        for row in summaries:
            writer.writerow(row)

    print(f"Wrote {summary_path}")
    for row in summaries:
        print(
            "threads={threads} samples={sampled_handoffs} offcpu={successor_offcpu_fraction} "
            "later={later_waiter_switch_fraction} median_gap={median_handoff_gap_ns} "
            "p95_gap={p95_handoff_gap_ns} p95_dispatch={p95_dispatch_delay_ns}".format(
                **row
            )
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
