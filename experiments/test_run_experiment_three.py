from __future__ import annotations

import csv
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import run_experiment_three as experiment_three  # noqa: E402
import experiment_defaults  # noqa: E402


RAW_FIELDS = (
    "threads",
    "critical_iters",
    "outside_iters",
    "repeat",
    "throughput_ops_per_sec",
    "elapsed_seconds",
    "total_operations",
    "avg_lock_hold_ns",
    "avg_wait_ns_estimated",
    "avg_lock_handoff_ns_estimated",
    "lock_hold_samples",
    "avg_cpu_pct",
)


def write_raw(root: Path, lock: str, *, threads: int = 1, critical: int = 100, outside: int = 0) -> None:
    lock_dir = root / lock
    lock_dir.mkdir(parents=True)
    with (lock_dir / "raw.csv").open("w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=RAW_FIELDS)
        writer.writeheader()
        writer.writerow(
            {
                "threads": str(threads),
                "critical_iters": str(critical),
                "outside_iters": str(outside),
                "repeat": "1",
                "throughput_ops_per_sec": "1.0",
                "elapsed_seconds": "4.000123",
                "total_operations": "4",
                "avg_lock_hold_ns": "1.0",
                "avg_wait_ns_estimated": "1.0",
                "avg_lock_handoff_ns_estimated": "1.0",
                "lock_hold_samples": "1",
                "avg_cpu_pct": "100.0",
            }
        )


class RunExperimentThreeTest(unittest.TestCase):
    def test_missing_experiment_locks_from_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for lock in ("flexguard", "hapax", "mcs", "mcs-tas", "mutex", "reciprocating"):
                write_raw(root, lock)

            self.assertEqual(
                experiment_three.missing_experiment_locks(root),
                (
                    "mcstp",
                    "mcs_tas_accordin_admission_only",
                    "mcs_tas_accordin_sampled",
                    "mcs_tas_accordin_no_admission",
                    "mcs_tas_accordin_taskset",
                    "mcs_extension",
                    "malthusian",
                ),
            )

    def test_baseline_matrix_is_inferred_from_existing_raw_csv(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_raw(root, "mcs", threads=1, critical=100, outside=0)
            write_raw(root, "mcs-tas", threads=2, critical=300, outside=100)

            matrix = experiment_three.infer_baseline_matrix(root)

            self.assertEqual(matrix.threads, (1, 2))
            self.assertEqual(matrix.critical_ns, (100, 300))
            self.assertEqual(matrix.outside_ns, (0, 100))
            self.assertEqual(matrix.repeats, 1)
            self.assertEqual(matrix.duration_ms, 4000)

    def test_old_common_helpers_remain_reexported(self) -> None:
        self.assertTrue(hasattr(experiment_three, "CommandLogger"))
        self.assertTrue(hasattr(experiment_three, "with_sudo_env"))

    def test_default_output_root_is_under_experiments_results(self) -> None:
        self.assertEqual(experiment_three.DEFAULT_OUTPUT_ROOT.parent.name, "results")
        self.assertEqual(experiment_three.DEFAULT_OUTPUT_ROOT.parent.parent.name, "experiments")

    def test_experiment_threads_replace_baseline_threads_without_caps(self) -> None:
        matrix = experiment_three.BaselineMatrix(
            threads=(1, 2),
            critical_ns=(100,),
            outside_ns=(0,),
            repeats=3,
            duration_ms=4000,
        )

        matrix = experiment_three.matrix_with_threads(matrix, experiment_defaults.DEFAULT_THREADS)

        self.assertEqual(matrix.threads, experiment_defaults.DEFAULT_THREADS)
        self.assertEqual(
            experiment_three.runnable_threads_for_lock("mcstp", matrix),
            experiment_defaults.DEFAULT_THREADS,
        )

    def test_force_explicit_lock_runs_even_when_output_is_complete(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_raw(root, "mcs_tas_accordin_taskset", threads=4, critical=100, outside=0)
            matrix = experiment_three.BaselineMatrix(
                threads=(4,),
                critical_ns=(100,),
                outside_ns=(0,),
                repeats=1,
                duration_ms=4000,
            )

            self.assertEqual(
                experiment_three.resolve_requested_locks(
                    root,
                    root,
                    "mcs_tas_accordin_taskset",
                    matrix,
                    force=False,
                ),
                (),
            )
            self.assertEqual(
                experiment_three.resolve_requested_locks(
                    root,
                    root,
                    "mcs_tas_accordin_taskset",
                    matrix,
                    force=True,
                ),
                ("mcs_tas_accordin_taskset",),
            )

    def test_taskset_target_cpu_count_uses_outside_over_critical_plus_one(self) -> None:
        self.assertEqual(experiment_three.taskset_target_cpu_count(300, 3000, 96), 11)
        self.assertEqual(experiment_three.taskset_target_cpu_count(300, 10000, 96), 34)
        self.assertEqual(experiment_three.taskset_target_cpu_count(3000, 10000, 96), 4)
        self.assertEqual(experiment_three.taskset_target_cpu_count(100, 10000, 96), 96)

    def test_taskset_cpu_list_spills_from_first_numa_node_to_second(self) -> None:
        topology = experiment_three.TasksetTopology(
            source="test",
            nodes=(
                experiment_three.TasksetNode(node=0, cpus=(0, 2, 4)),
                experiment_three.TasksetNode(node=1, cpus=(1, 3, 5)),
            ),
        )

        self.assertEqual(experiment_three.taskset_cpu_list(topology, 2), (0, 2))
        self.assertEqual(experiment_three.taskset_cpu_list(topology, 5), (0, 2, 4, 1, 3))

    def test_taskset_sweeps_are_split_by_critical_outside_target(self) -> None:
        topology = experiment_three.TasksetTopology(
            source="test",
            nodes=(
                experiment_three.TasksetNode(node=0, cpus=(0, 2, 4)),
                experiment_three.TasksetNode(node=1, cpus=(1, 3, 5)),
            ),
        )
        matrix = experiment_three.BaselineMatrix(
            threads=(4, 8),
            critical_ns=(100, 300),
            outside_ns=(10000,),
            repeats=3,
            duration_ms=4000,
        )

        sweeps = experiment_three.taskset_sweep_specs(matrix, topology)

        self.assertEqual(
            [(s.critical_ns, s.outside_ns, s.target_cpus, s.cpu_list) for s in sweeps],
            [
                (100, 10000, 6, "0,2,4,1,3,5"),
                (300, 10000, 6, "0,2,4,1,3,5"),
            ],
        )
        self.assertEqual(sweeps[0].matrix.critical_ns, (100,))
        self.assertEqual(sweeps[0].matrix.outside_ns, (10000,))


if __name__ == "__main__":
    unittest.main()
