#!/usr/bin/env python3
"""Fail when a `scripts/bench-real` run regressed against the run recorded on main.

    python3 scripts/bench-check.py docs/bench-real.csv target/bench-real/results.csv

Both CSVs are read as scripts/render-assets.py reads them (median of the `ok` samples).
Each workload is compared as scion's median over git's from the same run, so a run on
another machine compares with the recorded one: a row regressed when that ratio grew by
more than 20%. A row Scion failed, or no longer measured, regressed too. The nightly
`bench` job in .github/workflows/ci.yml runs this on each slice.
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

LIMIT = 1.2
# Below this many milliseconds a change is process start and timer jitter, not code: a
# 6 ms row that measures 8 ms is +33% and means nothing.
SLACK_MS = 5


def load_render_assets():
    sys.dont_write_bytecode = True  # no __pycache__ next to the scripts
    path = Path(__file__).resolve().parent / "render-assets.py"
    spec = importlib.util.spec_from_file_location("render_assets", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__.strip().splitlines()[2].strip(), file=sys.stderr)
        return 2
    assets = load_render_assets()
    _, base = assets.bench_real(Path(sys.argv[1]))
    _, new = assets.bench_real(Path(sys.argv[2]))
    regressed = []
    print(f"{'workload':<24} {'main':>8} {'this run':>9} {'change':>8}")
    for key, _ in assets.BENCH_WORKLOADS:
        git0, scion0 = base.get((key, "git")), base.get((key, "scion"))
        if not (isinstance(git0, int) and isinstance(scion0, int)):
            continue
        git1, scion1 = new.get((key, "git")), new.get((key, "scion"))
        if not (isinstance(git1, int) and isinstance(scion1, int)):
            print(f"{key:<24} {scion0 / git0:>7.2f}x {'failed':>9}")
            regressed.append(key)
            continue
        before, after = scion0 / git0, scion1 / git1
        change = after / before
        bad = change > LIMIT and scion1 - before * git1 > SLACK_MS
        print(f"{key:<24} {before:>7.2f}x {after:>8.2f}x {change - 1:>+8.0%}{'  regressed' if bad else ''}")
        if bad:
            regressed.append(key)
    if regressed:
        print(f"regressed more than {LIMIT - 1:.0%} against main: {', '.join(regressed)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
