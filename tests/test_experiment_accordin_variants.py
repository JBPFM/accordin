from __future__ import annotations

import sys
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
EXPERIMENTS_DIR = REPO_ROOT / "experiments"
sys.path.insert(0, str(EXPERIMENTS_DIR))

import experiment_defaults  # noqa: E402
import run_experiment_one  # noqa: E402
import run_experiment_four  # noqa: E402
import run_experiment_five  # noqa: E402
import run_experiment_six  # noqa: E402


ACCORDIN_VARIANTS = (
    "mcs_tas_accordin_admission_only",
    "mcs_tas_accordin_sampled",
    "mcs_tas_accordin_no_admission",
    "mcs_tas_accordin_taskset",
)
LEVELDB_ACCORDIN_DIRECT_VARIANTS = (
    "mcs_tas_accordin_direct_admission_only",
    "mcs_tas_accordin_direct_sampled",
    "mcs_tas_accordin_direct_no_admission",
    "mcs_tas_accordin_direct_taskset",
)


def sudo_env_tokens(command: list[str]) -> list[str]:
    assert command[:3] == ["sudo", "-n", "env"]
    tokens, _ = env_tokens_and_payload(command, 3)
    return tokens


def env_tokens_and_payload(command: list[str], start: int) -> tuple[list[str], list[str]]:
    tokens: list[str] = []
    index = start
    while index < len(command):
        token = command[index]
        if token == "-u":
            tokens.extend(command[index:index + 2])
            index += 2
        elif "=" in token:
            tokens.append(token)
            index += 1
        else:
            break
    return tokens, command[index:]


def sudo_env_assignments(command: list[str]) -> dict[str, str]:
    assignments: dict[str, str] = {}
    for token in sudo_env_tokens(command):
        if "=" not in token:
            continue
        key, value = token.split("=", 1)
        assignments[key] = value
    return assignments


def assert_removals_precede_assignments(test: unittest.TestCase, tokens: list[str]) -> None:
    first_assignment = next(
        (index for index, token in enumerate(tokens) if "=" in token),
        len(tokens),
    )
    later_removals = [
        token
        for token in tokens[first_assignment:]
        if token == "-u"
    ]
    test.assertEqual(later_removals, [])


def experiment_one_args() -> object:
    class Args:
        threads = (128,)
        repeats = 1
        mcs_accordin_taskset_cpus = experiment_defaults.DEFAULT_MCS_ACCORDIN_TASKSET_CPUS

    return Args()


class FakeLogger:
    def __init__(self) -> None:
        self.commands: list[list[str]] = []

    def run(self, cmd: list[str], **_: object) -> None:
        self.commands.append(cmd)


class AccordinVariantDefaultsTest(unittest.TestCase):
    def test_minimal_profile_expands_accordin_into_four_variants(self) -> None:
        self.assertEqual(
            experiment_defaults.MINIMAL_LOCKS,
            (*ACCORDIN_VARIANTS, "flexguard"),
        )
        for lock in ACCORDIN_VARIANTS:
            self.assertIn(lock, experiment_defaults.FULL_LOCKS)

    def test_accordin_aliases_select_specific_variants(self) -> None:
        self.assertEqual(
            experiment_defaults.normalize_lock("accordin"),
            "mcs_tas_accordin_admission_only",
        )
        self.assertEqual(
            experiment_defaults.normalize_lock("mcs_tas_accordin"),
            "mcs_tas_accordin_admission_only",
        )
        self.assertEqual(
            experiment_defaults.normalize_lock("accordin_admission_only"),
            "mcs_tas_accordin_admission_only",
        )
        self.assertEqual(
            experiment_defaults.normalize_lock("accordin_sampled"),
            "mcs_tas_accordin_sampled",
        )
        self.assertEqual(
            experiment_defaults.normalize_lock("accordin_no_admission"),
            "mcs_tas_accordin_no_admission",
        )
        self.assertEqual(
            experiment_defaults.normalize_lock("accordin_taskset"),
            "mcs_tas_accordin_taskset",
        )

    def test_default_accordin_concurrency_comes_from_taskset_cpu_list(self) -> None:
        self.assertEqual(experiment_defaults.DEFAULT_ACCORDIN_CONCURRENCY, 11)

    def test_old_no_bpf_keys_are_not_supported(self) -> None:
        for lock in ("accordin_no_bpf", "accordin_no_bpf_sampled", "accordin_no_sampling"):
            with self.subTest(lock=lock):
                with self.assertRaises(ValueError):
                    experiment_defaults.resolve_locks(
                        profile="minimal",
                        locks=(lock,),
                    )


class AccordinVariantCommandTest(unittest.TestCase):
    def test_admission_only_accordin_clears_controller_env(self) -> None:
        command, env = run_experiment_five.build_lock_command(
            "mcs_tas_accordin_admission_only",
            ["payload"],
        )

        self.assertIsNone(env)
        tokens = sudo_env_tokens(command)
        assert_removals_precede_assignments(self, tokens)
        assignments = sudo_env_assignments(command)
        self.assertIn("LD_PRELOAD", assignments)
        self.assertIn("-u", tokens)
        self.assertIn("K", tokens)
        self.assertIn("ACCORDIN_CPU_MASK_K", tokens)
        self.assertIn("MCS_TAS_ACCORDIN_DISABLE_BPF", tokens)
        self.assertNotIn("K", assignments)

    def test_parsec_submit_command_uses_same_taskset_variant_shape(self) -> None:
        import shlex

        command = shlex.split(run_experiment_six.submit_command_for_lock("mcs_tas_accordin_taskset"))

        self.assertEqual(command[:3], ["sudo", "-n", "env"])
        outer_tokens, payload = env_tokens_and_payload(command, 3)
        assert_removals_precede_assignments(self, outer_tokens)
        self.assertEqual(
            payload[:4],
            ["taskset", "-c", experiment_defaults.DEFAULT_MCS_ACCORDIN_TASKSET_CPUS, "env"],
        )
        inner_tokens, inner_payload = env_tokens_and_payload(payload, 4)
        inner_assignments = {
            key: value
            for key, value in (token.split("=", 1) for token in inner_tokens if "=" in token)
        }
        self.assertEqual(inner_payload, [])
        self.assertIn("LD_PRELOAD", inner_assignments)
        self.assertNotIn("LD_PRELOAD", sudo_env_assignments(command))

    def test_leveldb_accordin_variants_use_direct_preload_without_hook_scope(self) -> None:
        prefix, env = run_experiment_four.lock_command_prefix("mcs_tas_accordin_direct_sampled")

        self.assertEqual(prefix, [])
        self.assertIsNotNone(env)
        assert env is not None
        self.assertIsNone(env["ACCORDIN_HOOK_SCOPE"])
        self.assertEqual(env["K"], str(experiment_defaults.DEFAULT_ACCORDIN_CONCURRENCY))
        self.assertIsNone(env["ACCORDIN_CPU_MASK_K"])
        self.assertEqual(Path(env["LD_PRELOAD"]).name, "libmcs_tas_accordin_direct.so")

    def test_experiment_one_accordin_variant_uses_direct_mutexbench_lock(self) -> None:
        cmd, env = run_experiment_one.accordin_sweep_command(
            lock="mcs_tas_accordin_sampled",
            result_root=Path("/tmp/experiment1-test"),
            args=experiment_one_args(),
        )

        self.assertIsNotNone(env)
        assert env is not None
        self.assertNotIn("--bench-ld-preload", cmd)
        self.assertIn("--lock-kind", cmd)
        self.assertEqual(cmd[cmd.index("--lock-kind") + 1], "mcs_tas_accordin_direct")
        self.assertEqual(env["K"], str(experiment_defaults.DEFAULT_ACCORDIN_CONCURRENCY))
        self.assertEqual(
            Path(env["MCS_TAS_ACCORDIN_DIRECT_LIB"]).name,
            "libmcs_tas_accordin_direct.so",
        )
        self.assertNotIn("LD_PRELOAD", env)

    def test_experiment_one_controller_only_uses_stats_only_scheduler(self) -> None:
        cmd, env = run_experiment_one.accordin_sweep_command(
            lock="mcs_tas_accordin_no_admission",
            result_root=Path("/tmp/experiment1-test"),
            args=experiment_one_args(),
        )

        self.assertIsNotNone(env)
        assert env is not None
        self.assertNotIn("--bench-ld-preload", cmd)
        self.assertEqual(cmd[cmd.index("--lock-kind") + 1], "mcs_tas_accordin_direct")
        self.assertEqual(env["K"], str(experiment_defaults.DEFAULT_ACCORDIN_CONCURRENCY))
        self.assertEqual(env["ACCORDIN_DISABLE_ADMISSION"], "1")
        self.assertEqual(env["MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY"], "1")
        self.assertIsNone(env["MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF"])

    def test_experiment_one_taskset_variant_uses_direct_mutexbench_lock(self) -> None:
        cmd, env = run_experiment_one.accordin_sweep_command(
            lock="mcs_tas_accordin_taskset",
            result_root=Path("/tmp/experiment1-test"),
            args=experiment_one_args(),
        )

        self.assertIsNotNone(env)
        assert env is not None
        self.assertEqual(
            cmd[:3],
            ["taskset", "-c", experiment_defaults.DEFAULT_MCS_ACCORDIN_TASKSET_CPUS],
        )
        self.assertNotIn("--bench-ld-preload", cmd)
        self.assertNotIn("K", {key for key, value in env.items() if value is not None})
        self.assertEqual(
            Path(env["MCS_TAS_ACCORDIN_DIRECT_LIB"]).name,
            "libmcs_tas_accordin_direct.so",
        )

    def test_experiment_one_asks_make_to_refresh_mutexbench_binary(self) -> None:
        logger = FakeLogger()

        run_experiment_one.ensure_mutex_bench(logger)

        self.assertEqual(logger.commands[0][-1], "mutex_bench")

    def test_leveldb_minimal_profile_uses_direct_accordin_variants(self) -> None:
        self.assertEqual(
            run_experiment_four.resolve_leveldb_locks(profile="minimal", locks=None),
            (*LEVELDB_ACCORDIN_DIRECT_VARIANTS, "flexguard"),
        )
        self.assertEqual(
            run_experiment_four.normalize_leveldb_lock("mcs_tas_accordin_direct"),
            "mcs_tas_accordin_direct_admission_only",
        )

    def test_leveldb_direct_controller_only_uses_stats_only_scheduler(self) -> None:
        command, env = run_experiment_four.build_measured_command(
            lock="mcs_tas_accordin_direct_no_admission",
            db_bench=Path("db_bench"),
            db_path=Path("/tmp/leveldb-test"),
            benchmark="readrandom",
            threads=128,
            num=500000,
            reads=12288,
            use_existing_db=True,
        )

        self.assertIsNone(env)
        tokens = sudo_env_tokens(command)
        assignments = sudo_env_assignments(command)
        self.assertEqual(assignments["K"], str(experiment_defaults.DEFAULT_ACCORDIN_CONCURRENCY))
        self.assertEqual(assignments["ACCORDIN_DISABLE_ADMISSION"], "1")
        self.assertEqual(assignments["MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY"], "1")
        self.assertIn("MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF", tokens)
        self.assertNotIn("MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF", assignments)
        self.assertIn("LD_PRELOAD", assignments)
        self.assertEqual(Path(assignments["LD_PRELOAD"]).name, "libmcs_tas_accordin_direct.so")

    def test_sampled_accordin_sets_k_without_disabling_bpf(self) -> None:
        command, env = run_experiment_five.build_lock_command(
            "mcs_tas_accordin_sampled",
            ["payload"],
        )

        self.assertIsNone(env)
        tokens = sudo_env_tokens(command)
        assert_removals_precede_assignments(self, tokens)
        assignments = sudo_env_assignments(command)
        self.assertEqual(assignments["K"], str(experiment_defaults.DEFAULT_ACCORDIN_CONCURRENCY))
        self.assertIn("ACCORDIN_CPU_MASK_K", tokens)
        self.assertIn("MCS_TAS_ACCORDIN_DISABLE_BPF", tokens)
        self.assertNotIn("MCS_TAS_ACCORDIN_DISABLE_BPF", assignments)

    def test_controller_only_accordin_uses_stats_only_scheduler(self) -> None:
        command, env = run_experiment_five.build_lock_command(
            "mcs_tas_accordin_no_admission",
            ["payload"],
        )

        self.assertIsNone(env)
        tokens = sudo_env_tokens(command)
        assert_removals_precede_assignments(self, tokens)
        assignments = sudo_env_assignments(command)
        self.assertEqual(assignments["K"], str(experiment_defaults.DEFAULT_ACCORDIN_CONCURRENCY))
        self.assertEqual(assignments["ACCORDIN_DISABLE_ADMISSION"], "1")
        self.assertEqual(assignments["MCS_TAS_ACCORDIN_STATS_ONLY"], "1")
        self.assertIn("MCS_TAS_ACCORDIN_DISABLE_BPF", tokens)
        self.assertNotIn("MCS_TAS_ACCORDIN_DISABLE_BPF", assignments)

    def test_taskset_accordin_uses_best_cpu_list_without_sampling(self) -> None:
        command, env = run_experiment_five.build_lock_command(
            "mcs_tas_accordin_taskset",
            ["payload"],
        )

        self.assertIsNone(env)
        outer_tokens, payload = env_tokens_and_payload(command, 3)
        assert_removals_precede_assignments(self, outer_tokens)
        outer_assignments = sudo_env_assignments(command)
        self.assertNotIn("LD_PRELOAD", outer_assignments)
        self.assertEqual(
            payload[:4],
            ["taskset", "-c", experiment_defaults.DEFAULT_MCS_ACCORDIN_TASKSET_CPUS, "env"],
        )
        inner_tokens, inner_payload = env_tokens_and_payload(payload, 4)
        assert_removals_precede_assignments(self, inner_tokens)
        inner_assignments = {
            key: value
            for key, value in (token.split("=", 1) for token in inner_tokens if "=" in token)
        }
        self.assertEqual(inner_payload, ["payload"])
        self.assertIn("LD_PRELOAD", inner_assignments)
        self.assertNotIn("K", inner_assignments)


if __name__ == "__main__":
    unittest.main()
