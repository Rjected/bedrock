#!/usr/bin/env python3
"""Compare Bedrock SVM exit counters at two guest workload boundaries."""

import argparse
import json
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path, help="marker snapshot JSON")
    parser.add_argument("after", type=Path, help="later exit-stats JSON")
    args = parser.parse_args()

    before = json.loads(args.before.read_text())
    after = json.loads(args.after.read_text())

    def delta(name: str) -> int:
        return after[name] - before[name]

    exits = []
    for name, value in before.items():
        if isinstance(value, dict) and "count" in value and "cycles" in value:
            count = after[name]["count"] - value["count"]
            cycles = after[name]["cycles"] - value["cycles"]
            exits.append((name, count, cycles))

    total_exits = sum(count for _, count, _ in exits)
    total_handler = sum(cycles for _, _, cycles in exits)
    total_run = delta("total_run_cycles")

    print(f"VM exits: {total_exits:,}; run cycles: {total_run:,}")
    print("\nExit types by count:")
    for name, count, cycles in sorted(exits, key=lambda row: row[1], reverse=True):
        if count:
            print(
                f"  {name:20} {count:>12,} ({count / total_exits:6.1%})"
                f"  {cycles:>16,} cycles  {cycles // count:>7,} cycles/exit"
            )

    phases = [
        ("VM runner and guest", delta("guest_cycles")),
        ("VM-entry preparation", delta("vmentry_overhead_cycles")),
        ("VM-exit bookkeeping", delta("vmexit_overhead_cycles")),
        ("IRQ window", delta("irq_window_cycles")),
        ("Exit handlers", total_handler),
    ]
    phases.append(("Other / accounting gap", total_run - sum(value for _, value in phases)))
    print("\nHost-cycle breakdown:")
    for name, cycles in phases:
        print(f"  {name:24} {cycles:>16,}  {cycles / total_run:6.1%}")


if __name__ == "__main__":
    main()
