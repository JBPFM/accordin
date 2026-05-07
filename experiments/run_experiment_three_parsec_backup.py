#!/usr/bin/env python3
from __future__ import annotations

from typing import Sequence

import run_experiment_three_common as experiment_three
from run_experiment_three_common import *  # noqa: F401,F403


def main(argv: Sequence[str] | None = None) -> int:
    return experiment_three.main(
        argv,
        description="Run or plot the PARSEC dedup and streamcluster experiment sweep.",
        result_prefix="experiment3",
    )


if __name__ == "__main__":
    raise SystemExit(main())
