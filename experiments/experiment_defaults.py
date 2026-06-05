from __future__ import annotations

from dataclasses import dataclass
from typing import Iterable

try:
    from machine_config import (
        ACTIVE_MACHINE_CONFIG,
        DEFAULT_MCS_ACCORDIN_TASKSET_CPUS,
        DEFAULT_THREAD_COUNTS,
        MACHINE_PHYSICAL_CORES,
        PROFILE_ENV,
        limit_to_single_oversubscribed_thread_count,
    )
except ModuleNotFoundError:
    from .machine_config import (
        ACTIVE_MACHINE_CONFIG,
        DEFAULT_MCS_ACCORDIN_TASKSET_CPUS,
        DEFAULT_THREAD_COUNTS,
        MACHINE_PHYSICAL_CORES,
        PROFILE_ENV,
        limit_to_single_oversubscribed_thread_count,
    )


MACHINE_CORE_COUNT = MACHINE_PHYSICAL_CORES
DEFAULT_THREADS = DEFAULT_THREAD_COUNTS
ACCORDIN_ADMISSION_ONLY_LOCK = "mcs_tas_accordin_admission_only"
ACCORDIN_BASE_LOCK = ACCORDIN_ADMISSION_ONLY_LOCK
ACCORDIN_SAMPLED_LOCK = "mcs_tas_accordin_sampled"
ACCORDIN_NO_ADMISSION_LOCK = "mcs_tas_accordin_no_admission"
ACCORDIN_TASKSET_LOCK = "mcs_tas_accordin_taskset"
MCS_ACCORDIN_LOCK = "mcs_accordin"
PTHREAD_SPINLOCK_LOCK = "pthread_spinlock"
ACCORDIN_VARIANT_LOCKS = (
    ACCORDIN_BASE_LOCK,
)
ACCORDIN_SAMPLED_LOCKS: tuple[str, ...] = ()
ACCORDIN_ADMISSION_DISABLED_LOCKS: tuple[str, ...] = ()
ACCORDIN_TASKSET_LOCKS: tuple[str, ...] = ()


def _cpu_list_count(cpu_list: str) -> int:
    cpus: set[int] = set()
    for part in cpu_list.split(","):
        part = part.strip()
        if not part:
            continue
        if "-" in part:
            start_text, end_text = part.split("-", 1)
            start = int(start_text)
            end = int(end_text)
            cpus.update(range(start, end + 1))
        else:
            cpus.add(int(part))
    return len(cpus)


DEFAULT_ACCORDIN_CONCURRENCY = _cpu_list_count(DEFAULT_MCS_ACCORDIN_TASKSET_CPUS)

BASELINE_LOCKS = (
    "mutex",
    "mcs",
    "mcs_extension",
    "reciprocating",
    "flexguard",
    "cna",
    "gcr",
)
OTHERLOCKS_INTERPOSE_LOCKS = ("cna", "gcr")
FULL_LOCKS = (
    *BASELINE_LOCKS,
    *ACCORDIN_VARIANT_LOCKS,
)
MINIMAL_LOCKS = ACCORDIN_VARIANT_LOCKS
LOCK_PROFILES = {
    "minimal": MINIMAL_LOCKS,
    "full": FULL_LOCKS,
}
DEFAULT_LOCK_PROFILE = "minimal"
DEFAULT_LOCKS = LOCK_PROFILES[DEFAULT_LOCK_PROFILE]
DEFAULT_REPEATS = 3

MUTEXBENCH_DEFAULT_CRITICAL_NS = 300
MUTEXBENCH_DEFAULT_OUTSIDE_NS = 3000
MUTEXBENCH_DEFAULT_DURATION_MS = 5000
MUTEXBENCH_DEFAULT_WARMUP_DURATION_MS = 1000
MUTEXBENCH_DEFAULT_REPEATS = 4

EXPERIMENT_TWO_DEFAULT_LOCKS = ("mcs_tse", "mcs_tas")
EXPERIMENT_TWO_STYLE_THREADS = (1, 2, 4, 8, 16, 32, 64, 96, 128, 192, 256)
EXPERIMENT_ONE_DECOMPOSITION_LOCKS = (
    "mcs",
    "mcs_extension",
    "mcs-tas",
    "malthusian",
    "flexguard",
    ACCORDIN_BASE_LOCK,
)
EXPERIMENT_ONE_BASE_LOCKS = (
    "mcs",
    "mcstp",
    "mcs-tas",
    "reciprocating",
    "malthusian",
    "flexguard",
)
EXPERIMENT_ONE_ACCORDIN_LOCKS = ACCORDIN_VARIANT_LOCKS
EXPERIMENT_ONE_FOCUS_LOCKS = (ACCORDIN_BASE_LOCK, "flexguard")
EXPERIMENT_ONE_SUPPLEMENT_DEFAULT_LOCKS = ("reciprocating", "malthusian")

SINGLE_OVERSUBSCRIBED_LOCKS = (
    PTHREAD_SPINLOCK_LOCK,
    "mcs",
    "mcs_extension",
    "reciprocating",
    "cna",
    "gcr",
)

LOCK_LABELS = {
    "stock": "Stock",
    "mutex": "Mutex",
    "pthread": "Mutex",
    "pthread_spinlock": "Pthread spinlock",
    "mcs": "MCS",
    "mcstp": "MCS-TP",
    "mcs-tas": "MCS-TAS",
    "mcs_tas": "MCS-TAS",
    "mcstas": "MCS-TAS",
    "mcs_tse": "MCS + TSE",
    "mcs_extension": "MCS + TSE",
    "flexguard": "FlexGuard",
    "malthusian": "Malthusian",
    "reciprocating": "Reciprocating",
    "cna": "CNA",
    "gcr": "GCR",
    "accordin": "Accordin",
    "mcs_accordin": "MCS Accordin",
    "mcs_tas_accordin": "Accordin",
    "mcs_tas_accordin_admission_only": "Accordin",
}

LOCK_ALIASES = {
    "accordin": ACCORDIN_ADMISSION_ONLY_LOCK,
    "accordin_admission_only": ACCORDIN_ADMISSION_ONLY_LOCK,
    "mcs_tas_accordin": ACCORDIN_ADMISSION_ONLY_LOCK,
    "mcs_tas_accordin_admission_only": ACCORDIN_ADMISSION_ONLY_LOCK,
    "mcs_accordin": MCS_ACCORDIN_LOCK,
    "mcs-tas": "mcstas",
    "mcs_tas": "mcstas",
    "mcstas": "mcstas",
    "mcs_extension": "mcs_extension",
    "mcs-extension": "mcs_extension",
    "mcs-tse": "mcs_extension",
    "mcs_tse": "mcs_extension",
    "mutex": "mutex",
    "pthread": "mutex",
    "stock": "mutex",
    "pthread_spinlock": PTHREAD_SPINLOCK_LOCK,
    "pthread-spinlock": PTHREAD_SPINLOCK_LOCK,
    "cna": "cna",
    "gcr": "gcr",
}

EXPERIMENT_TWO_LOCK_ALIASES = {
    "mcs-tas": "mcs_tas",
    "mcstas": "mcs_tas",
    "mcs_tas": "mcs_tas",
    "mcs_tse": "mcs_tse",
    "mcs-tse": "mcs_tse",
    "mcs_extension": "mcs_extension",
    "mcs-extension": "mcs_extension",
}

LOCK_ORDER = (
    "mutex",
    "stock",
    "pthread_spinlock",
    "mcs",
    "mcs-tas",
    "mcs_tas",
    "mcstas",
    "mcs_tse",
    "mcs_extension",
    "reciprocating",
    "flexguard",
    "cna",
    "gcr",
    "mcstp",
    "malthusian",
    "mcs_tas_accordin_admission_only",
    "mcs_tas_accordin",
    "mcs_tas_accordin_sampled",
    "mcs_tas_accordin_no_admission",
    "mcs_tas_accordin_taskset",
    "mcs_accordin",
    "accordin",
)


@dataclass(frozen=True)
class ExperimentOneLockConfig:
    label: str
    key: str
    optional: bool = False
    result_dirs: tuple[str, ...] = ()


EXPERIMENT_ONE_LOCKS = (
    ExperimentOneLockConfig("MCS", "mcs"),
    ExperimentOneLockConfig("MCS-TP", "mcstp"),
    ExperimentOneLockConfig("MCS-TAS", "mcs-tas"),
    ExperimentOneLockConfig("Reciprocating", "reciprocating", optional=True),
    ExperimentOneLockConfig("Admission only", ACCORDIN_BASE_LOCK, optional=True),
    ExperimentOneLockConfig("MCS + TSE", "mcs_extension"),
    ExperimentOneLockConfig("Malthusian", "malthusian", optional=True),
    ExperimentOneLockConfig("FlexGuard", "flexguard"),
)

EXPERIMENT_ONE_FULL_LOCKS = tuple(config.key for config in EXPERIMENT_ONE_LOCKS)
EXPERIMENT_ONE_MINIMAL_LOCKS = ACCORDIN_VARIANT_LOCKS
EXPERIMENT_ONE_LOCK_PROFILES = {
    "minimal": EXPERIMENT_ONE_MINIMAL_LOCKS,
    "full": EXPERIMENT_ONE_FULL_LOCKS,
}


def lock_profile_names() -> tuple[str, ...]:
    return tuple(LOCK_PROFILES)


def lock_profile_locks(profile: str) -> tuple[str, ...]:
    try:
        return LOCK_PROFILES[profile]
    except KeyError as exc:
        supported = ", ".join(lock_profile_names())
        raise ValueError(f"Unsupported lock profile: {profile}. Supported profiles: {supported}.") from exc


def experiment_one_lock_profile_names() -> tuple[str, ...]:
    return tuple(EXPERIMENT_ONE_LOCK_PROFILES)


def experiment_one_lock_profile_locks(profile: str) -> tuple[str, ...]:
    try:
        return EXPERIMENT_ONE_LOCK_PROFILES[profile]
    except KeyError as exc:
        supported = ", ".join(experiment_one_lock_profile_names())
        raise ValueError(f"Unsupported lock profile: {profile}. Supported profiles: {supported}.") from exc


def normalize_lock(lock: str) -> str:
    normalized = lock.strip().lower()
    return LOCK_ALIASES.get(normalized, normalized)


def validate_locks(locks: tuple[str, ...]) -> tuple[str, ...]:
    supported = (
        set(FULL_LOCKS)
        | set(MINIMAL_LOCKS)
        | set(LOCK_ALIASES.values())
        | set(LOCK_LABELS)
    )
    unsupported = [lock for lock in locks if lock not in supported]
    if unsupported:
        raise ValueError(
            f"Unsupported lock keys: {', '.join(unsupported)}. "
            f"Supported keys: {', '.join(sorted(supported))}"
        )
    return locks


def normalize_locks(locks: Iterable[str]) -> tuple[str, ...]:
    return validate_locks(tuple(dict.fromkeys(normalize_lock(lock) for lock in locks)))


def resolve_locks(*, profile: str, locks: Iterable[str] | None) -> tuple[str, ...]:
    raw_locks = lock_profile_locks(profile) if locks is None else tuple(locks)
    return normalize_locks(raw_locks)


def normalize_experiment_two_lock(lock: str) -> str:
    normalized = lock.strip().lower()
    try:
        return EXPERIMENT_TWO_LOCK_ALIASES[normalized]
    except KeyError as exc:
        supported = "mcs_tse, mcs_tas/mcs-tas/mcstas, mcs_extension"
        raise ValueError(f"Unsupported lock key: {lock}. Supported locks: {supported}.") from exc


def lock_label(lock: str) -> str:
    return LOCK_LABELS.get(lock, lock)


def lock_sort_key(lock: str) -> tuple[int, str]:
    if lock in LOCK_ORDER:
        return (LOCK_ORDER.index(lock), lock)
    return (len(LOCK_ORDER), lock)


def is_accordin_lock(lock: str) -> bool:
    return lock in ACCORDIN_VARIANT_LOCKS


def is_mcs_accordin_lock(lock: str) -> bool:
    return lock == MCS_ACCORDIN_LOCK


def accordin_uses_sampling(lock: str) -> bool:
    return lock in ACCORDIN_SAMPLED_LOCKS


def accordin_disables_admission(lock: str) -> bool:
    return lock in ACCORDIN_ADMISSION_DISABLED_LOCKS


def accordin_uses_taskset(lock: str) -> bool:
    return lock in ACCORDIN_TASKSET_LOCKS


def is_otherlocks_interpose_lock(lock: str) -> bool:
    return lock in OTHERLOCKS_INTERPOSE_LOCKS


def runnable_threads_for_lock(lock: str, threads: tuple[int, ...]) -> tuple[int, ...]:
    if lock not in SINGLE_OVERSUBSCRIBED_LOCKS:
        return threads
    return limit_to_single_oversubscribed_thread_count(threads, MACHINE_CORE_COUNT)


def per_lock_max_threads_for_settings(
    locks: tuple[str, ...],
    threads: tuple[int, ...],
) -> dict[str, int]:
    return {
        lock: max(runnable_threads_for_lock(lock, threads))
        for lock in locks
        if lock in SINGLE_OVERSUBSCRIBED_LOCKS
    }
