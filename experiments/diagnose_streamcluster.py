#!/usr/bin/env python3
"""Reproduce Streamcluster overruns without changing its workload or lock policy."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import time

import run as suite


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build", type=Path, default=suite.ROOT / "target/experiments")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--cpus")
    parser.add_argument("--threads", type=int, help="default: four times the selected physical cores")
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--budget", type=float, default=20, help="original smoke deadline, retained as an overrun flag")
    parser.add_argument("--timeout", type=float, default=90, help="diagnostic process deadline")
    parser.add_argument("--custody", choices=("on", "off"), default="on")
    parser.add_argument("--spread", action="store_true")
    args = parser.parse_args()
    machine = suite.topology(args.cpus)
    threads = args.threads if args.threads is not None else 4 * machine["P"]
    if threads < 1 or args.repeats < 1 or not 0 < args.budget <= args.timeout:
        parser.error("positive threads/repeats and 0 < budget <= timeout are required")
    if os.geteuid() != 0:
        parser.error("Accordin BPF diagnosis needs root")
    build = args.build.resolve()
    manifest = json.loads((build / "manifest.json").read_text())
    for path, expected in manifest["sha256"].items():
        if suite.sha(path) != expected:
            parser.error(f"artifact changed after prepare.py: {path}")
    lockpath = Path("/tmp/mutexbench-sweep-multi-lock.lock")
    try:
        fd = os.open(lockpath, os.O_CREAT | os.O_EXCL | os.O_RDONLY, 0o644)
    except FileExistsError:
        fd = os.open(lockpath, os.O_RDONLY)
    with os.fdopen(fd, "r") as lockfile:
        fcntl.flock(lockfile, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if suite.sched_state() != "disabled":
            parser.error("another sched_ext scheduler is active")
        out = args.out.resolve()
        out.mkdir(parents=True, exist_ok=False)
        env = suite.lock_env("accordin", manifest)
        env.update(ACCORDIN_CV_COUNTERS="1", ACCORDIN_CV_CUSTODY=str(int(args.custody == "on")),
                   ACCORDIN_CV_FLUSH_FLAGS="24" if args.spread else "8")
        params = suite.PROFILES["smoke"]
        config = dict(machine=machine, threads=threads, parameters=params,
                      diagnostic_timeout=args.timeout, original_budget=args.budget,
                      repeats=args.repeats, script_sha256=suite.sha(__file__),
                      environment={k: v for k, v in env.items() if k.startswith(("LD_", "ACCORDIN_", "MCS_"))})
        (out / "config.json").write_text(json.dumps(config, indent=2) + "\n")
        shutil.copy2(build / "manifest.json", out / "manifest.json")
        failures = 0
        with (out / "results.jsonl").open("w") as results:
            for repeat in range(args.repeats):
                case = out / f"run-{repeat}"
                case.mkdir()
                command, _ = suite.workload_command("streamcluster", threads, params, manifest, case, None)
                command = ["taskset", "-c", ",".join(map(str, machine["cpus"]))] + command
                row = dict(repeat=repeat, timestamp=time.time())
                row.update(suite.execute(command, case, env, case / "log.txt", args.timeout,
                                         manifest["locks"]["accordin"], "accordin"))
                row["exceeded_original_budget"] = row["wall_seconds"] > args.budget
                text = (case / "log.txt").read_text()
                match = re.search(r"^\[accordin_cv\] (.+)$", text, re.M)
                if match:
                    row["cv"] = {k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", match[1])}
                if row["status"] == "ok":
                    try:
                        row.update(suite.metrics("streamcluster", text, None, case, params))
                        cv = row.get("cv", {})
                        if not cv or cv["parked"] != sum(cv[k] for k in ("flushed", "expired", "drained", "parked_now")):
                            raise ValueError("missing or unbalanced custody counters")
                    except (ValueError, KeyError, OSError) as error:
                        row.update(status="invalid", reason=str(error))
                        row.pop("rate", None)
                failures += row["status"] != "ok" or row["exceeded_original_budget"]
                results.write(json.dumps(row) + "\n")
                results.flush()
                print(json.dumps(row), flush=True)
        # Completing under the diagnostic deadline does not pass the original
        # smoke deadline. Preserve that failure in the exit status as well.
        return int(failures != 0)


if __name__ == "__main__":
    raise SystemExit(main())
