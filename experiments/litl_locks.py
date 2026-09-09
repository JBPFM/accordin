"""Resolution of the LiTL launchers that put a lock algorithm in front of a process.

Every baseline lock algorithm the experiments compare against lives in the LiTL
checkout. A LiTL launcher is a shell wrapper that appends the algorithm's shared
object to LD_PRELOAD and execs the command it is given, so the process below it
sees the algorithm on every pthread_mutex_* call and the benchmark itself only
ever needs a plain pthread mutex.

This module is the single place that knows where the checkout is, which LiTL
algorithm implements which experiment lock name, and how to build one.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Callable, Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_LITL_DIR = REPO_ROOT / "third_party" / "litl"
LITL_DIR_ENV = "LITL_DIR"

# Baseline locks whose algorithm comes from LiTL. The launcher is the whole
# mechanism: they carry no environment of their own and need no root.
LITL_BASELINE_ALGORITHMS = {
    "mcs": "mbmcs_original",
    "mcs_tas": "mbmcstas_original",
    # The microbenchmark used to hold an rseq time-slice extension across the
    # critical section of whichever lock it measured. That combination is now
    # a LiTL algorithm of its own.
    "mcs_extension": "mbmcstse_original",
    "reciprocating": "mbreciprocating_original",
    "cna": "cna_original",
    "gcr": "gcr_original",
}

# Accordin locks reach a process through a LiTL launcher too, but they keep
# their own environment variables and their scheduler still needs root.
LITL_ACCORDIN_ALGORITHMS = {
    "accordin": "mcstasaccordin_original",
    "mcs_tas_accordin": "mcstasaccordin_original",
    "mcs_tas_accordin_admission_only": "mcstasaccordin_original",
    "mcs_tas_accordin_no_admission": "mcstasaccordin_original",
    "mcs_tas_accordin_taskset": "mcstasaccordin_original",
    "mcs_tas_accordin_direct": "mcstasaccordin_original",
    "mcs_accordin": "mcsaccordin_original",
    "mcs_accordin_direct": "mcsaccordin_original",
}

LITL_ALGORITHMS = {**LITL_BASELINE_ALGORITHMS, **LITL_ACCORDIN_ALGORITHMS}
LITL_BASELINE_LOCKS: tuple[str, ...] = tuple(LITL_BASELINE_ALGORITHMS)


def litl_dir() -> Path:
    override = os.environ.get(LITL_DIR_ENV)
    if override:
        return Path(override).expanduser().resolve()
    return DEFAULT_LITL_DIR


def is_litl_interpose_lock(lock: str) -> bool:
    """True for the baseline locks whose only mechanism is a LiTL launcher."""
    return lock in LITL_BASELINE_ALGORITHMS


def is_litl_lock(lock: str) -> bool:
    """True for every lock name that maps onto a LiTL algorithm."""
    return lock in LITL_ALGORITHMS


def litl_algorithm(lock: str) -> str:
    try:
        return LITL_ALGORITHMS[lock]
    except KeyError as exc:
        supported = ", ".join(sorted(LITL_ALGORITHMS))
        raise ValueError(
            f"No LiTL algorithm is mapped for lock {lock}. Mapped locks: {supported}."
        ) from exc


def litl_launcher(lock: str) -> Path:
    return litl_dir() / f"lib{litl_algorithm(lock)}.sh"


def litl_library(lock: str) -> Path:
    return litl_dir() / "lib" / f"lib{litl_algorithm(lock)}.so"


def litl_command_prefix(lock: str) -> list[str]:
    """Command prefix that runs its arguments under this lock's algorithm."""
    return [str(litl_launcher(lock))]


def build_command(lock: str) -> list[str]:
    return ["make", "-C", str(litl_dir()), f"ALGORITHMS={litl_algorithm(lock)}", "all"]


def artifact_error(lock: str) -> str | None:
    launcher = litl_launcher(lock)
    library = litl_library(lock)
    if not launcher.is_file():
        return f"{lock} LiTL launcher is missing: {launcher}"
    if not os.access(launcher, os.X_OK):
        return f"{lock} LiTL launcher is not executable: {launcher}"
    if not library.is_file():
        return f"{lock} LiTL library is missing: {library}"
    return None


def is_built(lock: str) -> bool:
    return artifact_error(lock) is None


def ensure_built(
    lock: str,
    run: Callable[[Sequence[str]], None],
    *,
    verify: bool = True,
) -> None:
    """Build the LiTL algorithm behind `lock` unless its artifacts are present.

    `run` receives the build command; callers pass whatever runner they already
    use so the build is logged the way the rest of their build steps are. With
    `verify` false the artifacts are not checked, which is what a driver printing
    a plan rather than running it needs.
    """
    if verify and is_built(lock):
        return
    run(build_command(lock))
    if not verify:
        return
    error = artifact_error(lock)
    if error is not None:
        raise RuntimeError(f"LiTL build did not produce the expected artifacts: {error}")
