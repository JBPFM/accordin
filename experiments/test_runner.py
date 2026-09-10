"""Small tests for measurement validity, independent of benchmark builds/root."""
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import litl_locks
import prepare
import run


# The algorithm each compared lock resolves to, spelled out rather than read
# back from the table under test.
EXPECTED_ALGORITHMS = {
    "mcs": "mbmcs_original",
    "mcs-tas": "mbmcstas_original",
    "gcr": "gcr_original",
    "flexguard": "flexguard_original",
    "mcs-tse": "mbmcstse_original",
    "accordin": "mcstasaccordin_original",
}


class LockTableTests(unittest.TestCase):
    def test_compared_locks_resolve_to_the_expected_algorithms(self):
        self.assertEqual(list(run.LOCKS), list(EXPECTED_ALGORITHMS))
        for lock, algorithm in EXPECTED_ALGORITHMS.items():
            self.assertEqual(litl_locks.litl_algorithm(lock), algorithm)

    def test_unmapped_lock_is_rejected(self):
        with self.assertRaises(ValueError):
            litl_locks.litl_algorithm("cna")

    def test_preloaded_library_is_the_one_prepare_records(self):
        build = Path("/build/experiments-litl")
        # prepare.py writes exactly this into manifest["locks"].
        manifest = {"locks": litl_locks.manifest_locks(build / "litl")}
        for lock in run.LOCKS:
            expected = build / "litl" / "lib" / f"lib{EXPECTED_ALGORITHMS[lock]}.so"
            self.assertEqual(manifest["locks"][lock], str(expected))
            self.assertEqual(run.lock_env(lock, manifest)["LD_PRELOAD"], str(expected))


class MeasurementTests(unittest.TestCase):
    def test_throughput_uses_aggregate_elapsed(self):
        output = "readrandom : 300 micros/op\nEXP_RESULT name=readrandom ops=400 seconds=2.0\n"
        result = run.metrics("leveldb-readrandom", output, 400, Path("."), {})
        self.assertEqual(result["rate"], 200)
        with self.assertRaises(ValueError):
            run.metrics("leveldb-readrandom", output, 401, Path("."), {})

    def test_timeout_remains_missing_in_summary(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            (out / "config.json").write_text(json.dumps({"machine": {"P": 4}}))
            rows = [dict(workload="rocksdb", lock="mcs", threads=4, phase="measure", status="ok", rate=100),
                    dict(workload="rocksdb", lock="mcs", threads=16, phase="measure", status="timeout"),
                    dict(workload="rocksdb", lock="accordin", threads=16, phase="measure", status="ok", rate=200)]
            run.report(out, rows)
            summary = json.loads((out / "summary.json").read_text())["rows"]
            self.assertIsNone(summary[1]["median_rate"])
            self.assertIsNone(summary[2]["speedup_vs_mcs"])
            self.assertEqual(summary[1]["timeouts"], 1)

    def test_remaining_attempts_of_a_timed_out_configuration_are_skipped(self):
        previous = [dict(workload="streamcluster", lock="mcs", threads=192, phase="warmup", repeat=0,
                         status="timeout"),
                    dict(workload="streamcluster", lock="mcs", threads=96, phase="warmup", repeat=0,
                         status="ok")]
        timed_out = run.timed_out_configurations(previous)
        self.assertEqual(timed_out, {("streamcluster", "mcs", 192)})
        for configuration in [("streamcluster", "mcs", 96), ("streamcluster", "accordin", 192),
                              ("raytrace", "mcs", 192)]:
            self.assertNotIn(configuration, timed_out)

    def test_a_timeout_recorded_during_the_run_skips_the_later_phases(self):
        # Reading the rule back from the recorded rows is what --resume does.
        recorded, executed = [], []
        for phase, repeat, status in [("warmup", 0, "ok"), ("measure", 0, "timeout"), ("measure", 1, "ok"),
                                      ("measure", 2, "ok")]:
            if ("rocksdb", "mcs", 96) in run.timed_out_configurations(recorded):
                status = "skipped"
            executed.append((phase, repeat, status))
            recorded.append(dict(workload="rocksdb", lock="mcs", threads=96, phase=phase,
                                 repeat=repeat, status=status))
        self.assertEqual(executed, [("warmup", 0, "ok"), ("measure", 0, "timeout"),
                                    ("measure", 1, "skipped"), ("measure", 2, "skipped")])

    def test_report_counts_skipped_separately(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            (out / "config.json").write_text(json.dumps({"machine": {"P": 4}}))
            rows = [dict(workload="rocksdb", lock="mcs", threads=16, phase="measure", status="timeout"),
                    dict(workload="rocksdb", lock="mcs", threads=16, phase="measure", status="skipped",
                         reason=run.SKIP_REASON),
                    dict(workload="rocksdb", lock="mcs", threads=4, phase="measure", status="ok", rate=100)]
            counts = run.report(out, rows)
            self.assertEqual(counts["skipped"], 1)
            self.assertEqual(counts["timeout"], 1)
            summary = {(r["workload"], r["lock"], r["threads"]): r
                       for r in json.loads((out / "summary.json").read_text())["rows"]}
            overloaded = summary[("rocksdb", "mcs", 16)]
            self.assertEqual((overloaded["skipped"], overloaded["timeouts"], overloaded["completed"]), (1, 1, 0))
            self.assertIsNone(overloaded["median_rate"])
            self.assertEqual(summary[("rocksdb", "mcs", 4)]["skipped"], 0)
            self.assertIn("skipped", (out / "summary.csv").read_text().splitlines()[0].split(","))

    def test_driver_scripts_hash_under_their_own_manifest_key(self):
        layout = prepare.manifest_digests(
            [Path("/repo/target/experiments-litl/lock_probe"), Path("/usr/lib/libc.so.6")],
            [Path("/repo/experiments/run.py"), Path("/repo/experiments/litl_locks.py")],
            digest=lambda path: path.name)
        self.assertEqual(layout["sha256"],
                         {"/repo/target/experiments-litl/lock_probe": "lock_probe",
                          "/usr/lib/libc.so.6": "libc.so.6"})
        self.assertEqual(layout["drivers_sha256"],
                         {"/repo/experiments/run.py": "run.py",
                          "/repo/experiments/litl_locks.py": "litl_locks.py"})

    def test_cpu_ranges_and_reject_reversed(self):
        self.assertEqual(run.cpulist("0-3,8,2"), [0, 1, 2, 3, 8])
        with self.assertRaises(ValueError):
            run.cpulist("8-2")

    def test_ray_rejects_missing_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            p = Path(directory) / "image.rl"
            p.write_bytes((128).to_bytes(4, "big") * 2 + bytes([1, 2, 3, 127]) * 128)
            self.assertEqual(run.validate_ray(p)["height"], 128)
            p.write_bytes(p.read_bytes()[:-4])
            with self.assertRaises(ValueError):
                run.validate_ray(p)

    def test_execute_timeout_and_kills_child_group(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            with patch.object(run, "sched_state", return_value="disabled"), patch.object(run, "sched_seq", return_value=0):
                row = run.execute(["sleep", "10"], out, run.clean_env(), out / "log", .05)
            self.assertEqual(row["status"], "timeout")
            self.assertLess(row["wall_seconds"], 3)


if __name__ == "__main__":
    unittest.main()
