#!/usr/bin/env python3
from __future__ import annotations

from typing import Sequence

import run_experiment_three_common as experiment_three


def main(argv: Sequence[str] | None = None) -> int:
    return experiment_three.main(
        argv,
        fixed_benchmarks=("streamcluster",),
        result_prefix="experiment3_streamcluster",
        description="Run or plot the PARSEC streamcluster experiment sweep.",
    )


if __name__ == "__main__":
    raise SystemExit(main())
