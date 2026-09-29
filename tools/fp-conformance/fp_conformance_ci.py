#!/usr/bin/env python3
"""CI entrypoint that keeps inline Rust test modules outside the FP budget.

The scanner already exempts files under tests/spec/fixtures paths because test
code intentionally exercises effects and panic assertions. Rust commonly keeps
those tests inline under `#[cfg(test)] mod tests { ... }`; this wrapper applies
that same documented policy to inline test modules without changing production
source findings or the recorded budget.
"""

from __future__ import annotations

import re
import sys

import fp_conformance as scanner

_CFG_TEST = re.compile(r"^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*$")
_MODULE_OPEN = re.compile(r"^\s*mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{")


def _without_inline_rust_tests(ctx: scanner.FileContext) -> scanner.FileContext:
    if ctx.lang != "rust":
        return ctx

    lines = list(ctx.lines)
    code_lines = set(ctx.code_lines)
    awaiting_test_module = False
    test_depth = 0

    for index, line in enumerate(lines):
        line_number = index + 1

        if test_depth > 0:
            test_depth += line.count("{") - line.count("}")
            lines[index] = ""
            code_lines.discard(line_number)
            if test_depth <= 0:
                test_depth = 0
            continue

        if _CFG_TEST.match(line):
            awaiting_test_module = True
            lines[index] = ""
            code_lines.discard(line_number)
            continue

        if not awaiting_test_module:
            continue

        if not line.strip():
            continue

        if line.lstrip().startswith("#["):
            lines[index] = ""
            code_lines.discard(line_number)
            continue

        if _MODULE_OPEN.match(line):
            test_depth = line.count("{") - line.count("}")
            lines[index] = ""
            code_lines.discard(line_number)

        awaiting_test_module = False

    return scanner.FileContext(
        path=ctx.path,
        rel=ctx.rel,
        lang=ctx.lang,
        lines=tuple(lines),
        is_effect_boundary=ctx.is_effect_boundary,
        is_stateful=ctx.is_stateful,
        code_lines=frozenset(code_lines),
    )


_ORIGINAL_BUILD_CONTEXT = scanner.build_context


def _build_context(path: str, rel: str, lang: str) -> scanner.FileContext | None:
    ctx = _ORIGINAL_BUILD_CONTEXT(path, rel, lang)
    if ctx is None:
        return None
    return _without_inline_rust_tests(ctx)


scanner.build_context = _build_context


if __name__ == "__main__":
    try:
        sys.exit(scanner.main(sys.argv[1:]))
    except KeyboardInterrupt:
        sys.exit(130)
