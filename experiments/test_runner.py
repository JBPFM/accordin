"""Small tests for measurement validity, independent of benchmark builds/root."""
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import litl_locks
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
        self.assertEqual(set(run.LOCKS), set(EXPECTED_ALGORITHMS))
        self.assertEqual(len(run.LOCKS), len(EXPECTED_ALGORITHMS))
        for lock, algorithm in EXPECTED_ALGORITHMS.items():
            self.assertEqual(litl_locks.litl_algorithm(lock), algorithm)
        self.assertEqual(len(set(EXPECTED_ALGORITHMS.values())), len(EXPECTED_ALGORITHMS))

    def test_unmapped_lock_is_rejected(self):
        with self.assertRaises(ValueError):
            litl_locks.litl_algorithm("cna")

    def test_build_algorithms_drops_repeats(self):
        # mcs_tas_accordin_direct and accordin share mcstasaccordin_original.
        self.assertEqual(litl_locks.build_algorithms(["accordin", "mcs_tas_accordin_direct"]),
                         ["mcstasaccordin_original"])
        self.assertEqual(litl_locks.build_algorithms(run.LOCKS),
                         sorted(set(EXPECTED_ALGORITHMS.values())))

    def test_preloaded_library_is_the_one_prepare_records(self):
        build = Path("/build/experiments-litl")
        # prepare.py writes exactly this into manifest["locks"].
        manifest = {"locks": {lock: str(litl_locks.litl_library(lock, build / "litl"))
                              for lock in litl_locks.EXPERIMENT_LOCKS}}
        for lock in run.LOCKS:
            expected = build / "litl" / "lib" / f"lib{EXPECTED_ALGORITHMS[lock]}.so"
            self.assertEqual(manifest["locks"][lock], str(expected))
            self.assertEqual(run.lock_env(lock, manifest)["LD_PRELOAD"], str(expected))

    def test_library_and_launcher_follow_the_named_checkout(self):
        directory = Path("/build/litl")
        self.assertEqual(litl_locks.litl_library("gcr", directory),
                         directory / "lib" / "libgcr_original.so")
        self.assertEqual(litl_locks.litl_launcher("gcr", directory),
                         directory / "libgcr_original.sh")


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
            (out / "results.jsonl").write_text("\n".join(map(json.dumps, rows)))
            run.report(out)
            summary = json.loads((out / "summary.json").read_text())["rows"]
            self.assertIsNone(summary[1]["median_rate"])
            self.assertIsNone(summary[2]["speedup_vs_mcs"])
            self.assertEqual(summary[1]["timeouts"], 1)

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
