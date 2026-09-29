#!/usr/bin/env python3
"""Fail only when FP findings regress relative to the checked-out base tree.

The static budget is retained as historical documentation, but CI compares the
PR/push head to its actual first parent. This prevents stale budgets from
blocking unrelated changes while still making every rule/path count a ratchet:
a PR may reduce or preserve an existing backlog, but may not add another
occurrence in any source path.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from pathlib import Path


def _load(path: Path) -> Counter[tuple[str, str]]:
    with path.open(encoding="utf-8") as handle:
        payload = json.load(handle)

    findings = payload.get("findings")
    if not isinstance(findings, list):
        raise ValueError(f"{path} does not contain a findings list")

    counts: Counter[tuple[str, str]] = Counter()
    for finding in findings:
        if not isinstance(finding, dict):
            raise ValueError(f"{path} contains a non-object finding")
        code = finding.get("code")
        source_path = finding.get("path")
        if not isinstance(code, str) or not isinstance(source_path, str):
            raise ValueError(f"{path} contains an invalid finding identity")
        counts[(code, source_path)] += 1
    return counts


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="compare FP findings to a base tree")
    parser.add_argument("base_json", type=Path)
    parser.add_argument("head_json", type=Path)
    args = parser.parse_args(argv)

    try:
        base = _load(args.base_json)
        head = _load(args.head_json)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"fp-conformance-diff: invalid input: {error}", file=sys.stderr)
        return 2

    regressions = []
    for identity, now in sorted(head.items()):
        before = base.get(identity, 0)
        if now > before:
            regressions.append((identity[0], identity[1], before, now))

    if regressions:
        print("fp-conformance-diff: regression against exact base tree")
        for code, source_path, before, now in regressions:
            print(f"  {code} {source_path}: {before} -> {now}")
        return 1

    print("fp-conformance-diff: no new rule/path findings versus exact base tree")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
