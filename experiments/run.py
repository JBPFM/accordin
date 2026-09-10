#!/usr/bin/env python3
"""Serial six-application overload sweep. Run prepare.py first, then sudo this file."""
import argparse
import csv
import datetime as dt
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import shutil
import signal
import statistics
import subprocess
import sys
import time

import litl_locks
from litl_locks import sha256

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
WORKLOADS = ("leveldb-readrandom", "leveldb-fillrandom", "streamcluster", "raytrace", "kyoto-cachedb", "rocksdb")
LOCKS = litl_locks.EXPERIMENT_LOCKS
PROFILES = {
    "full": dict(keys=100000, read_ops=1966080, write_ops=983040, kyoto_ops=1966080,
                 value_bytes=32, stream=[10, 30, 512, 32768, 32768, 2000]),
    "smoke": dict(keys=4096, read_ops=49152, write_ops=24576, kyoto_ops=49152,
                  value_bytes=32, stream=[10, 20, 16, 512, 512, 1000]),
}


SKIP_REASON = "an earlier attempt of this configuration timed out"


def resume_state(rows):
    """Attempts already recorded, and the configurations one of them timed out on."""
    done, timed_out = set(), set()
    for row in rows:
        done.add((row["workload"], row["lock"], row["threads"], row["phase"], row["repeat"]))
        if row.get("status") == "timeout":
            timed_out.add((row["workload"], row["lock"], row["threads"]))
    return done, timed_out


def timed_out_configurations(rows):
    """The (workload, lock, threads) triples with at least one recorded timeout."""
    return resume_state(rows)[1]


def verify_artifacts(manifest, when):
    """Every measured artifact must still hash to what prepare.py recorded."""
    for file, expected in manifest["sha256"].items():
        if sha256(file) != expected:
            raise RuntimeError(f"artifact changed {when}: {file}; rerun prepare.py")


def cpulist(value):
    cpus = set()
    for part in value.split(","):
        bounds = part.split("-")
        if len(bounds) == 1:
            cpus.add(int(part))
        elif len(bounds) == 2 and int(bounds[0]) <= int(bounds[1]):
            cpus.update(range(int(bounds[0]), int(bounds[1]) + 1))
        else:
            raise ValueError(f"invalid CPU list: {value}")
    if not cpus or min(cpus) < 0:
        raise ValueError("empty or negative CPU set")
    return sorted(cpus)


def topology(requested=None):
    allowed = os.sched_getaffinity(0)
    cores = {}
    for cpu in sorted(allowed):
        base = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology")
        core = (int((base / "physical_package_id").read_text()), int((base / "core_id").read_text()))
        cores.setdefault(core, []).append(cpu)
    selected = cpulist(requested) if requested else sorted(v[0] for v in cores.values())
    if not set(selected) <= allowed:
        raise ValueError("--cpus must be a subset of the current process affinity")
    if any(len(set(v) & set(selected)) > 1 for v in cores.values()):
        raise ValueError("select one hardware thread per physical core; SMT siblings change the meaning of P")
    p = len(selected)
    return {"P": p, "cpus": selected, "allowed_logical_cpus": sorted(allowed),
            "physical_cores_available": len(cores), "smt_policy": "one logical CPU per physical core",
            "threads": sorted(set([max(1, p // 4), max(1, p // 2), p, 2*p, 4*p]))}


def read_file(path, default=""):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return default


def sched_state():
    return read_file("/sys/kernel/sched_ext/state", "unavailable")


def sched_seq():
    return int(read_file("/sys/kernel/sched_ext/enable_seq", "0"))


def clean_env():
    prefixes = ("LD_", "ACCORDIN_", "MCS_", "MCSTAS_", "GCR_", "FLEXGUARD_", "SCX_", "LITL_")
    env = {k: v for k, v in os.environ.items() if not k.startswith(prefixes)}
    env.update(LC_ALL="C", OMP_NUM_THREADS="1", OPENBLAS_NUM_THREADS="1")
    return env


def lock_env(lock, manifest):
    env = clean_env()
    env["LD_PRELOAD"] = manifest["locks"][lock]
    if lock == "accordin":
        env.update(MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF="0", MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY="0",
                   ACCORDIN_DISABLE_ADMISSION="0", ACCORDIN_AUTO_ADMISSION="0", ACCORDIN_CV_CUSTODY="1",
                   ACCORDIN_CV_CUSTODY_MS="20", ACCORDIN_CV_COUNTERS="0", ACCORDIN_CV_FLUSH_FLAGS="8",
                   ACCORDIN_CV_FLUSH_WIDTH="0", ACCORDIN_OWN_LIMIT="0", ACCORDIN_OWN_SLACK_US="100",
                   ACCORDIN_GROUP_SIZE="8")
    return env


def execute(command, cwd, env, log, timeout, expected_lib=None, lock=None):
    """Kill the complete process group on timeout; collect evidence outside the ROI."""
    # Apply affinity before injecting the interposer. Preloading taskset itself
    # would attach Accordin twice, once in taskset and again after exec.
    env = env.copy()
    if "LD_PRELOAD" in env:
        preload = env.pop("LD_PRELOAD")
        insert = 3 if command[0] == "taskset" else 0
        command = command[:insert] + ["env", f"LD_PRELOAD={preload}"] + command[insert:]
    before = sched_seq()
    if sched_state() not in ("disabled", "unavailable"):
        raise RuntimeError("another sched_ext scheduler is active")
    started = time.monotonic()
    observed = {"preload_seen": False, "direct_seen": False, "bpf_fd_seen": False,
                "sched_ext_enabled_seen": False, "peak_threads": 0}
    status = "ok"
    with log.open("w") as output:
        proc = subprocess.Popen(command, cwd=cwd, env=env, stdout=output, stderr=subprocess.STDOUT,
                                start_new_session=True)
        try:
            while proc.poll() is None:
                maps = read_file(f"/proc/{proc.pid}/maps")
                observed["preload_seen"] |= bool(expected_lib and str(expected_lib) in maps)
                observed["direct_seen"] |= "libmcs_tas_accordin_direct.so" in maps
                observed["sched_ext_enabled_seen"] |= sched_state() == "enabled"
                match = re.search(r"^Threads:\s+(\d+)", read_file(f"/proc/{proc.pid}/status"), re.M)
                if match:
                    observed["peak_threads"] = max(observed["peak_threads"], int(match[1]))
                if lock in ("accordin", "flexguard") and not observed["bpf_fd_seen"]:
                    try:
                        observed["bpf_fd_seen"] = any("bpf" in os.readlink(p) for p in Path(f"/proc/{proc.pid}/fd").iterdir())
                    except OSError:
                        pass
                if time.monotonic() - started >= timeout:
                    status = "timeout"
                    os.killpg(proc.pid, signal.SIGTERM)
                    try:
                        proc.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        os.killpg(proc.pid, signal.SIGKILL)
                    break
                time.sleep(0.001 if time.monotonic() - started < 0.2 else 0.02)
            proc.wait()
        except BaseException:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
            raise
    elapsed = time.monotonic() - started
    deadline = time.monotonic() + 5
    while sched_state() == "enabled" and time.monotonic() < deadline:
        time.sleep(0.02)
    after = sched_seq()
    if sched_state() not in ("disabled", "unavailable"):
        raise RuntimeError(f"scheduler did not detach after {log}")
    reason = ""
    if status == "ok" and proc.returncode != 0:
        status, reason = "error", f"exit {proc.returncode}"
    if status == "ok" and expected_lib and not observed["preload_seen"]:
        status, reason = "invalid", "preload mapping was not observed"
    if status == "ok" and lock == "accordin" and not (
            observed["direct_seen"] and observed["bpf_fd_seen"] and
            observed["sched_ext_enabled_seen"] and after - before == 1):
        status, reason = "invalid", "Accordin library/BPF/sched_ext evidence incomplete"
    if status == "ok" and lock == "flexguard" and not observed["bpf_fd_seen"]:
        status, reason = "invalid", "FlexGuard BPF fd was not observed"
    if lock != "accordin" and after != before:
        status, reason = "invalid", "unexpected sched_ext enable_seq change"
    return dict(status=status, reason=reason, returncode=proc.returncode, wall_seconds=elapsed, executed_command=command,
                sched_seq_before=before, sched_seq_after=after, **observed)


def db_options(binary, db, params, rocks=False):
    command = [str(binary), f"--db={db}", f"--num={params['keys']}",
               f"--value_size={params['value_bytes']}", "--cache_size=268435456", "--block_size=4096"]
    if rocks:
        command += ["--compression_type=none", "--cache_type=lru_cache", "--cache_numshardbits=0",
                    "--disable_wal=1", "--max_background_jobs=2", "--progress_reports=0", "--seed=1000"]
    return command


def prepare_seeds(manifest, work, params, cpus, timeout):
    for engine in ("leveldb", "rocksdb"):
        if not (work / engine / "CURRENT").exists():
            cmd = db_options(manifest["binaries"][engine], work / engine, params, engine == "rocksdb")
            cmd += ["--benchmarks=fillseq,compact", "--threads=1", "--write_buffer_size=65536"]
            evidence = execute(["taskset", "-c", cpus] + cmd, work, clean_env(), work / f"seed-{engine}.log", timeout)
            if evidence["status"] != "ok":
                raise RuntimeError(f"seed failed: {engine}: {evidence}")


def workload_command(workload, threads, params, manifest, directory, seeds):
    bins = manifest["binaries"]
    expected_ops = None
    if workload.startswith("leveldb"):
        db = directory / "db"
        read = workload.endswith("readrandom")
        if read:
            shutil.copytree(seeds / "leveldb", db)
        cmd = db_options(bins["leveldb"], db, params)
        expected_ops = params["read_ops" if read else "write_ops"]
        cmd += [f"--threads={threads}", f"--total_ops={expected_ops}",
                "--write_buffer_size=268435456", "--use_existing_db=" + str(int(read)),
                "--benchmarks=" + ("readseq,readrandom" if read else "fillrandom")]
    elif workload == "rocksdb":
        shutil.copytree(seeds / "rocksdb", directory / "db")
        cmd = db_options(bins["rocksdb"], directory / "db", params, True)
        per_thread = max(1, params["read_ops"] // threads)
        expected_ops = per_thread * threads
        cmd += ["--benchmarks=readtocache,readrandom", "--use_existing_db=1", "--readonly=1", "--open_files=1000",
                f"--threads={threads}", f"--reads={per_thread}"]
    elif workload == "kyoto-cachedb":
        expected_ops = params["kyoto_ops"]
        cmd = [bins[workload], str(threads), str(params["keys"]), str(expected_ops), str(params["value_bytes"]), "10"]
    elif workload == "streamcluster":
        cmd = [bins[workload], *map(str, params["stream"]), "none", str(directory / "clusters.txt"), str(threads)]
    else:
        source = Path(manifest["build"]) / "inputs/raytrace"
        for name in ("car.env", "car.geo"):
            shutil.copy2(source / name, directory / name)
        cmd = [bins[workload], f"-p{threads}", "-a8", str(directory / "car.env")]
    return cmd, expected_ops


def validate_ray(path):
    # SPLASH2 RLE: big-endian width/height, then RGB and one-byte run length minus 1.
    data = path.read_bytes()
    if len(data) < 8:
        raise ValueError("raytrace output is truncated")
    width, height = int.from_bytes(data[:4], "big"), int.from_bytes(data[4:8], "big")
    if (width, height) != (128, 128):
        raise ValueError(f"unexpected raytrace dimensions {width}x{height}")
    # The precise encoder layout is checked below against its row boundaries.
    offset = 8
    for _ in range(height):
        pixels = 0
        while pixels < width:
            if offset + 4 > len(data):
                raise ValueError("truncated raytrace RLE row")
            pixels += data[offset + 3] + 1
            offset += 4
        if pixels != width:
            raise ValueError("raytrace RLE row overflow")
    if offset != len(data):
        raise ValueError("trailing raytrace output")
    return {"width": width, "height": height, "sha256": sha256(path)}


def metrics(workload, text, expected_ops, directory, params):
    if workload == "streamcluster":
        match = re.search(r"Benchmark time:\s*(\d+)", text)
        if not match:
            raise ValueError("missing Streamcluster ROI time")
        seconds = int(match[1]) / 1e9
        path = directory / "clusters.txt"
        lines = [line for line in path.read_text().splitlines() if line.strip()]
        if not lines or len(lines) % 3:
            raise ValueError("malformed cluster output")
        weight = 0.0
        for i in range(0, len(lines), 3):
            int(lines[i]); weight += float(lines[i+1])
            coords = [float(x) for x in lines[i+2].split()]
            if len(coords) != params["stream"][2] or not all(math.isfinite(x) for x in coords):
                raise ValueError("invalid cluster coordinates")
        if not math.isclose(weight, params["stream"][3], abs_tol=0.1):
            raise ValueError("cluster weights do not sum to input points")
        return {"seconds": seconds, "rate": 1 / seconds, "unit": "runs/s", "output_sha256": sha256(path)}
    if workload == "raytrace":
        match = re.search(r"Total time without initialization\s*[:=]?\s*(\d+)", text)
        if not match:
            raise ValueError("missing Raytrace ROI time")
        seconds = int(match[1]) / 1e6
        return {"seconds": seconds, "rate": 1 / seconds, "unit": "runs/s", "image": validate_ray(directory / "car.rl")}
    name = "fillrandom" if workload == "leveldb-fillrandom" else "cachedb" if workload == "kyoto-cachedb" else "readrandom"
    matches = re.findall(rf"EXP_RESULT name={name} ops=(\d+) seconds=([\d.]+)", text)
    if not matches:
        raise ValueError("missing aggregate operation count / ROI time")
    ops, seconds = int(matches[-1][0]), float(matches[-1][1])
    if ops != expected_ops or seconds <= 0 or not math.isfinite(seconds):
        raise ValueError(f"invalid measurement: ops={ops}, expected={expected_ops}, seconds={seconds}")
    return {"seconds": seconds, "ops": ops, "rate": ops / seconds, "unit": "ops/s"}


def report(out, records):
    groups = {}
    for row in records:
        if row["phase"] != "measure":
            continue
        groups.setdefault((row["workload"], row["lock"], row["threads"]), []).append(row)
    summary = []
    for (workload, lock, threads), rows in groups.items():
        valid = [r["rate"] for r in rows if r["status"] == "ok"]
        median = statistics.median(valid) if valid else None
        summary.append(dict(workload=workload, lock=lock, threads=threads, attempts=len(rows),
                            completed=len(valid), timeouts=sum(r["status"] == "timeout" for r in rows),
                            skipped=sum(r["status"] == "skipped" for r in rows),
                            unsupported=sum(r["status"] == "unsupported" for r in rows),
                            failures=sum(r["status"] in ("error", "invalid") for r in rows),
                            median_rate=median, min_rate=min(valid) if valid else None,
                            max_rate=max(valid) if valid else None,
                            cv=statistics.stdev(valid)/statistics.mean(valid) if len(valid)>1 else None))
    p = json.loads((out / "config.json").read_text())["machine"]["P"]
    at_p = {(r["workload"], r["lock"]): r["median_rate"] for r in summary if r["threads"] == p}
    mcs = {(r["workload"], r["threads"]): r["median_rate"] for r in summary if r["lock"] == "mcs"}
    for row in summary:
        base = at_p.get((row["workload"], row["lock"]))
        row["retention_vs_P"] = row["median_rate"] / base if base and row["median_rate"] else None
        base = mcs.get((row["workload"], row["threads"]))
        row["speedup_vs_mcs"] = row["median_rate"] / base if base and row["median_rate"] else None
    if summary:
        with (out / "summary.csv").open("w") as f:
            writer = csv.DictWriter(f, fieldnames=list(summary[0])); writer.writeheader(); writer.writerows(summary)
    counts = {status: sum(r["status"] == status for r in records)
              for status in ("ok", "timeout", "skipped", "error", "invalid", "unsupported")}
    (out / "summary.json").write_text(json.dumps({"counts": counts, "rows": summary}, indent=2) + "\n")
    return counts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build", type=Path, default=ROOT / "target/experiments")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--profile", choices=PROFILES, default="full")
    parser.add_argument("--cpus", help="one logical CPU per selected physical core; defaults to all available cores")
    parser.add_argument("--threads", type=int, nargs="+", help="explicit pilot subset; default P/4,P/2,P,2P,4P")
    parser.add_argument("--locks", choices=LOCKS, nargs="+", default=list(LOCKS))
    parser.add_argument("--workloads", choices=WORKLOADS, nargs="+", default=list(WORKLOADS))
    parser.add_argument("--repeats", type=int)
    parser.add_argument("--warmups", type=int)
    parser.add_argument("--timeout", type=float, help="per-process wall timeout including initialization")
    parser.add_argument("--seed", type=int, default=20260909, help="randomized serial order")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--allow-unsupported", action="store_true", help="record unsupported baselines but do not fail the exit code for them")
    parser.add_argument("--report-only", action="store_true")
    args = parser.parse_args()
    if args.report_only:
        records = [json.loads(line) for line in (args.out / "results.jsonl").read_text().splitlines()]
        print(json.dumps(report(args.out, records), indent=2)); return
    machine = topology(args.cpus)
    threads = sorted(set(args.threads or machine["threads"]))
    repeats = args.repeats if args.repeats is not None else (1 if args.profile == "smoke" else 3)
    warmups = args.warmups if args.warmups is not None else (0 if args.profile == "smoke" else 1)
    timeout = args.timeout if args.timeout is not None else (30 if args.profile == "smoke" else 600)
    if min(threads) < 1 or repeats < 1 or warmups < 0 or timeout <= 0:
        parser.error("threads, repeats and timeout must be positive; warmups must be nonnegative")
    if max(threads) > 1024:
        parser.error("Raytrace supports at most 1024 workers; use --cpus to select a smaller core set")
    plan = dict(profile=args.profile, machine=machine, threads=threads, locks=args.locks, workloads=args.workloads,
                parameters=PROFILES[args.profile], repeats=repeats, warmups=warmups, timeout=timeout, seed=args.seed)
    if args.dry_run:
        print(json.dumps(plan, indent=2)); return
    if os.geteuid() != 0 and any(lock in args.locks for lock in ("accordin", "flexguard")):
        parser.error("BPF benchmarks need root: sudo -n python3 experiments/run.py ...")
    build = args.build.resolve()
    manifest = json.loads((build / "manifest.json").read_text())
    verify_artifacts(manifest, "after prepare.py")
    # One digest over every driver script, so a row records the whole driver.
    driver_sha256 = hashlib.sha256("".join(manifest["drivers_sha256"].values()).encode()).hexdigest()
    out = (args.out or ROOT / "results" / ("overload-" + dt.datetime.now().strftime("%Y%m%d-%H%M%S"))).resolve()
    out.mkdir(parents=True, exist_ok=True)
    plan["manifest_sha256"] = sha256(build / "manifest.json")
    plan["build"] = str(build)
    config_path = out / "config.json"
    if config_path.exists():
        if not args.resume or json.loads(config_path.read_text()) != plan:
            raise RuntimeError("output exists; --resume requires exactly the same configuration and manifest")
    else:
        config_path.write_text(json.dumps(plan, indent=2) + "\n")
        shutil.copy2(build / "manifest.json", out / "manifest.json")
    results = out / "results.jsonl"
    results.touch(exist_ok=True)
    records = [json.loads(line) for line in results.read_text().splitlines()]
    done, timed_out = resume_state(records)
    jobs = []
    for phase, count in [("warmup", warmups), ("measure", repeats)]:
        for repeat in range(count):
            batch = [(w, l, t, phase, repeat) for w in args.workloads for t in threads for l in args.locks]
            random.Random(args.seed + repeat + (0 if phase == "warmup" else 10000)).shuffle(batch)
            jobs.extend(batch)
    print(f"P={machine['P']} CPUs={machine['cpus']} threads={threads}; {len(jobs)} attempts; {out}", flush=True)
    out.joinpath("logs").mkdir(exist_ok=True)
    out.joinpath("outputs").mkdir(exist_ok=True)
    # The same serialization convention is used by the existing repository sweeps.
    lockpath = Path("/tmp/mutexbench-sweep-multi-lock.lock")
    try:
        fd = os.open(lockpath, os.O_CREAT | os.O_EXCL | os.O_RDONLY, 0o644)
    except FileExistsError:
        fd = os.open(lockpath, os.O_RDONLY)  # Linux protected_regular rejects root O_CREAT on another user's /tmp file.
    with os.fdopen(fd, "r") as lockfile:
        try:
            fcntl.flock(lockfile, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError("another benchmark sweep holds the machine lock")
        if sched_state() not in ("disabled", "unavailable"):
            raise RuntimeError("another sched_ext scheduler is active")
        tse = subprocess.run([str(build / "tse_probe")], capture_output=True, text=True, env=clean_env())
        unsupported = {}
        if tse.returncode:
            unsupported["mcs-tse"] = tse.stdout.strip() + " " + tse.stderr.strip()
        if sched_state() == "unavailable":
            unsupported["accordin"] = "kernel does not expose sched_ext"
        (out / "host.json").write_text(json.dumps({"uname": platform.uname()._asdict(),
            "lscpu": subprocess.check_output(["lscpu"], text=True), "loadavg": read_file("/proc/loadavg"),
            "governors": sorted(set(p.read_text().strip() for p in Path("/sys/devices/system/cpu").glob("cpu*/cpufreq/scaling_governor"))),
            "sched_ext_initial": sched_state(), "unsupported": unsupported}, indent=2) + "\n")
        # Unique tmpfs directory; only this run's own data are deleted.
        import tempfile
        with tempfile.TemporaryDirectory(prefix="accordin-overload-", dir="/dev/shm") as scratch:
            work = Path(scratch)
            cpus = ",".join(map(str, machine["cpus"]))
            preflight = {}
            for lock in args.locks:
                if lock in unsupported:
                    continue
                command = ["taskset", "-c", cpus, str(build / "lock_probe"), manifest["locks"][lock]]
                evidence = execute(command, work, lock_env(lock, manifest), out / "logs" / f"probe-{lock}.log",
                                   60, manifest["locks"][lock], lock)
                preflight[lock] = evidence
                (out / "preflight.json").write_text(json.dumps(preflight, indent=2) + "\n")
                if evidence["status"] != "ok":
                    raise RuntimeError(f"lock correctness/binding preflight failed: {lock}: {evidence}")
            if any(w.startswith("leveldb") or w == "rocksdb" for w in args.workloads):
                prepare_seeds(manifest, work, PROFILES[args.profile], cpus, max(timeout, 120))
                for p in work.glob("seed-*.log"):
                    shutil.copy2(p, out / "logs" / p.name)
            for index, (workload, lock, count, phase, repeat) in enumerate(jobs):
                key = (workload, lock, count, phase, repeat)
                if key in done:
                    continue
                tag = f"{workload}-{lock}-t{count}-{phase}-{repeat}"
                row = dict(workload=workload, lock=lock, threads=count, phase=phase, repeat=repeat,
                           profile=args.profile, timestamp=dt.datetime.now(dt.timezone.utc).isoformat(),
                           driver_sha256=driver_sha256)
                print(f"[{index+1}/{len(jobs)}] {tag}", flush=True)
                if (workload, lock, count) in timed_out:
                    row.update(status="skipped", reason=SKIP_REASON)
                elif lock in unsupported:
                    row.update(status="unsupported", reason=unsupported[lock])
                else:
                    directory = work / "case"; directory.mkdir()
                    command, expected = workload_command(workload, count, PROFILES[args.profile], manifest, directory, work)
                    command = ["taskset", "-c", cpus] + command
                    env = lock_env(lock, manifest)
                    row.update(command=command, expected_ops=expected, log=f"logs/{tag}.log",
                               environment={k: v for k, v in env.items() if k.startswith(("LD_", "ACCORDIN_", "MCS_"))})
                    row.update(execute(command, directory, env, out / row["log"], timeout, manifest["locks"][lock], lock))
                    if row["status"] == "ok":
                        try:
                            row.update(metrics(workload, (out / row["log"]).read_text(errors="replace"), expected, directory, PROFILES[args.profile]))
                        except (ValueError, OSError, ZeroDivisionError) as error:
                            row.update(status="invalid", reason=str(error))
                    for name in ("clusters.txt", "car.rl"):
                        if (directory / name).exists():
                            shutil.copy2(directory / name, out / "outputs" / f"{tag}-{name}")
                    shutil.rmtree(directory)
                if row["status"] == "timeout":
                    timed_out.add((workload, lock, count))
                records.append(row)
                with results.open("a") as f:
                    f.write(json.dumps(row) + "\n"); f.flush(); os.fsync(f.fileno())
                print(f"  {row['status']} {row.get('seconds', '')} {row.get('reason', '')}", flush=True)
                report(out, records)
    counts = report(out, records)
    verify_artifacts(manifest, "during the run")
    (out / "audit.json").write_text(json.dumps({"artifacts_unchanged": True, "sched_ext_final": sched_state(),
          "expected_attempts": len(jobs), "recorded_attempts": sum(counts.values()), "counts": counts}, indent=2) + "\n")
    print(json.dumps(counts), flush=True)
    if counts["invalid"] or counts["error"] or counts["timeout"] or (counts["unsupported"] and not args.allow_unsupported):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
