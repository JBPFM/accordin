#!/usr/bin/env python3
"""Plot completed medians from run.py; missing/failed points remain gaps."""
import argparse
import json
from pathlib import Path
import math


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out", type=Path)
    args = parser.parse_args()
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    config = json.loads((args.out / "config.json").read_text())
    data = json.loads((args.out / "summary.json").read_text())
    p = config["machine"]["P"]
    for field, label, name in [("median_rate", "Throughput (ops/s or runs/s)", "throughput"),
                                ("retention_vs_P", "Throughput / same lock at P", "retention")]:
        fig, axes = plt.subplots(2, 3, figsize=(15, 8))
        for ax, workload in zip(axes.flat, config["workloads"]):
            for lock in config["locks"]:
                rows = {r["threads"]: r for r in data["rows"] if r["workload"] == workload and r["lock"] == lock}
                x = [t / p for t in config["threads"]]
                y = [rows.get(t, {}).get(field) or math.nan for t in config["threads"]]
                ax.plot(x, y, marker="o", label=lock)
            ax.set_xscale("log", base=2); ax.set_yscale("log")
            ax.set_xticks([.25, .5, 1, 2, 4], ["P/4", "P/2", "P", "2P", "4P"])
            ax.axvline(1, color="gray", linestyle=":")
            if field == "retention_vs_P": ax.axhline(1, color="gray", linestyle=":")
            ax.set_title(workload)
            ax.grid(alpha=.2)
        handles, labels = axes.flat[0].get_legend_handles_labels()
        fig.legend(handles, labels, loc="lower center", ncol=len(labels))
        fig.suptitle(f"{config['profile']} — P={p} — {label}")
        fig.text(.5, .045, "Completed medians only; gaps are missing data, not zero. Consult summary.csv for failures/timeouts.", ha="center", fontsize=9)
        fig.tight_layout(rect=[0, .08, 1, .94])
        for suffix in ("png", "svg"):
            fig.savefig(args.out / f"{name}.{suffix}", dpi=160)
        plt.close(fig)


if __name__ == "__main__":
    main()
