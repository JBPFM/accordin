#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import shutil
from pathlib import Path

import run_experiment_four as experiment_four


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Merge completed experiment-four chunks and recover rows from interrupted command logs."
    )
    parser.add_argument(
        "--commands-root",
        action="append",
        default=[],
        type=Path,
        help="Interrupted result root with commands.json and logs to recover successful rows from.",
    )
    parser.add_argument(
        "--raw-root",
        action="append",
        default=[],
        type=Path,
        help="Completed result root containing raw.csv to include.",
    )
    parser.add_argument(
        "--raw-parent",
        action="append",
        default=[],
        type=Path,
        help="Directory whose descendant raw.csv files should be included.",
    )
    parser.add_argument(
        "--output-root",
        required=True,
        type=Path,
        help="Output directory for merged raw.csv, summary.csv, settings.json, and plots.",
    )
    parser.add_argument(
        "--exclude-lock",
        action="append",
        default=[],
        help="Lock key to exclude from merged rows and missing-row checks. May be repeated.",
    )
    parser.add_argument("--force", action="store_true", help="Overwrite an existing merged output directory.")
    return parser.parse_args()


def command_arg(command: list[str], name: str) -> str | None:
    prefix = f"--{name}="
    for arg in command:
        if arg.startswith(prefix):
            return arg[len(prefix) :]
    return None


def command_flag_int(command: list[str], name: str) -> int:
    value = command_arg(command, name)
    if value is None:
        raise RuntimeError(f"Command is missing --{name}: {' '.join(command)}")
    return int(value)


def relative_or_absolute(root: Path, value: str) -> str:
    if not value:
        return ""
    path = Path(value)
    if path.is_absolute():
        return str(path)
    return str((root / path).resolve())


def load_raw_root(root: Path) -> list[dict[str, str]]:
    rows = experiment_four.load_raw_rows(root)
    for row in rows:
        row["init_command_log"] = relative_or_absolute(root, row["init_command_log"])
        row["command_log"] = relative_or_absolute(root, row["command_log"])
    return rows


def recover_command_rows(root: Path) -> list[dict[str, str]]:
    commands_path = root / "commands.json"
    settings_path = root / "settings.json"
    if not commands_path.is_file():
        raise RuntimeError(f"commands.json was not found under {root}")
    if not settings_path.is_file():
        raise RuntimeError(f"settings.json was not found under {root}")

    records = json.loads(commands_path.read_text(encoding="utf-8"))
    settings = json.loads(settings_path.read_text(encoding="utf-8"))
    total_ops = int(settings["total_ops"])
    fill_benchmark = settings.get("fill_benchmark", experiment_four.DEFAULT_FILL_BENCHMARK)

    records_by_log = {
        Path(str(record["log_path"])).name: record
        for record in records
        if record.get("log_path")
    }

    rows: list[dict[str, str]] = []
    for record in records:
        if record.get("returncode") != 0:
            continue
        command = [str(arg) for arg in record.get("command", [])]
        benchmark = command_arg(command, "benchmarks")
        if benchmark is None or benchmark == fill_benchmark:
            continue

        log_path = Path(str(record["log_path"]))
        log_name = log_path.name
        if log_name.startswith("init_") or not log_name.endswith(".log"):
            continue

        stem = log_name[:-4]
        prefix = f"{benchmark}_"
        if not stem.startswith(prefix):
            raise RuntimeError(f"Unexpected log name for benchmark {benchmark}: {log_name}")
        lock_thread_repeat = stem[len(prefix) :]
        lock, thread_text, repeat_text = lock_thread_repeat.rsplit("_", 2)
        if not repeat_text.startswith("r"):
            raise RuntimeError(f"Unexpected repeat suffix in log name: {log_name}")

        threads = int(thread_text)
        repeat = int(repeat_text[1:])
        db_bench_num = command_flag_int(command, "num")
        reads = command_arg(command, "reads")
        reads_per_thread = "" if reads is None else reads
        effective_total_ops = int(reads) * threads if reads is not None else db_bench_num * threads
        use_existing_db = command_arg(command, "use_existing_db") == "1"

        output = log_path.read_text(encoding="utf-8", errors="replace")
        latency = experiment_four.parse_latency(output, benchmark)

        init_record = None
        init_log_name = f"init_{stem}.log"
        if use_existing_db:
            init_record = records_by_log.get(init_log_name)
            if init_record is None or init_record.get("returncode") != 0:
                raise RuntimeError(f"Missing successful init log for {log_name}")

        rows.append(
            {
                "benchmark": benchmark,
                "lock": lock,
                "threads": str(threads),
                "repeat": str(repeat),
                "num": str(settings["num"]),
                "total_ops": str(total_ops),
                "db_bench_num": str(db_bench_num),
                "reads_per_thread": reads_per_thread,
                "effective_total_ops": str(effective_total_ops),
                "use_existing_db": "1" if use_existing_db else "0",
                "fill_benchmark": fill_benchmark if use_existing_db else "",
                "init_wall_seconds": (
                    "" if init_record is None else experiment_four.format_float(float(init_record["wall_seconds"]))
                ),
                "latency_micros_per_op": experiment_four.format_float(latency),
                "wall_seconds": experiment_four.format_float(float(record["wall_seconds"])),
                "init_command_log": "" if init_record is None else str(Path(str(init_record["log_path"])).resolve()),
                "command_log": str(log_path.resolve()),
            }
        )
    return rows


def filtered_settings(settings: dict, excluded_locks: set[str]) -> dict:
    if not excluded_locks:
        return settings
    settings = dict(settings)
    settings["locks"] = [lock for lock in settings["locks"] if lock["key"] not in excluded_locks]
    settings["per_lock_max_threads"] = {
        lock: max_threads
        for lock, max_threads in settings.get("per_lock_max_threads", {}).items()
        if lock not in excluded_locks
    }
    settings["runnable_threads_by_lock"] = {
        lock: threads
        for lock, threads in settings["runnable_threads_by_lock"].items()
        if lock not in excluded_locks
    }
    return settings


def expected_keys(settings: dict) -> set[tuple[str, str, str, str]]:
    repeats = int(settings["repeats"])
    keys: set[tuple[str, str, str, str]] = set()
    for benchmark in settings["benchmarks"]:
        benchmark_key = benchmark["key"]
        for lock in settings["locks"]:
            lock_key = lock["key"]
            for threads in settings["runnable_threads_by_lock"][lock_key]:
                for repeat in range(1, repeats + 1):
                    keys.add((benchmark_key, lock_key, str(threads), str(repeat)))
    return keys


def main() -> int:
    args = parse_args()
    output_root = experiment_four.resolve_path(args.output_root)
    excluded_locks = set(args.exclude_lock)
    if output_root.exists():
        if not args.force:
            raise RuntimeError(f"Output root already exists: {output_root}. Use --force to overwrite it.")
        shutil.rmtree(output_root)
    output_root.mkdir(parents=True)

    raw_roots = [experiment_four.resolve_path(root) for root in args.raw_root]
    for parent in args.raw_parent:
        resolved_parent = experiment_four.resolve_path(parent)
        raw_roots.extend(sorted(path.parent for path in resolved_parent.rglob("raw.csv")))

    rows_by_key: dict[tuple[str, str, str, str], dict[str, str]] = {}
    settings_source: Path | None = None
    for root_arg in args.commands_root:
        root = experiment_four.resolve_path(root_arg)
        if settings_source is None:
            settings_source = root / "settings.json"
        for row in recover_command_rows(root):
            if row["lock"] in excluded_locks:
                continue
            rows_by_key[(row["benchmark"], row["lock"], row["threads"], row["repeat"])] = row
    for root in raw_roots:
        if settings_source is None:
            settings_source = root / "settings.json"
        for row in load_raw_root(root):
            if row["lock"] in excluded_locks:
                continue
            rows_by_key[(row["benchmark"], row["lock"], row["threads"], row["repeat"])] = row

    if settings_source is None or not settings_source.is_file():
        raise RuntimeError("No settings.json source was found.")
    settings = filtered_settings(json.loads(settings_source.read_text(encoding="utf-8")), excluded_locks)
    with (output_root / "settings.json").open("w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")

    rows = sorted(
        rows_by_key.values(),
        key=lambda row: (
            experiment_four.benchmark_sort_key(row["benchmark"]),
            experiment_four.lock_sort_key(row["lock"]),
            int(row["threads"]),
            int(row["repeat"]),
        ),
    )
    raw_path = experiment_four.write_raw_csv(output_root, rows)
    summary_rows = experiment_four.summarize_rows(rows)
    summary_path = experiment_four.write_summary_csv(output_root, summary_rows)
    plot_paths = experiment_four.write_plots(output_root, summary_rows)

    missing = sorted(
        expected_keys(settings) - set(rows_by_key),
        key=lambda item: (
            experiment_four.benchmark_sort_key(item[0]),
            experiment_four.lock_sort_key(item[1]),
            int(item[2]),
            int(item[3]),
        ),
    )
    experiment_four.print_outputs(output_root, raw_path, summary_path, plot_paths)
    print(f"Merged rows: {len(rows)}")
    print(f"Missing rows: {len(missing)}")
    for benchmark, lock, threads, repeat in missing[:20]:
        print(f"Missing: benchmark={benchmark} lock={lock} threads={threads} repeat={repeat}")
    if len(missing) > 20:
        print(f"... {len(missing) - 20} more missing rows")
    return 0 if not missing else 3


if __name__ == "__main__":
    raise SystemExit(main())
