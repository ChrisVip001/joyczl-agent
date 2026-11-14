#!/usr/bin/env python3
"""The dashboard's two languages must cover the same strings.

The dictionary keys *are* the Chinese text (`t("新建会话")`), which keeps call sites
readable — but it also means nothing catches a missing entry: `t()` falls back to Chinese,
so an English screen quietly keeps a few Chinese labels. This script is the thing that
notices.

Three checks:

1. **Every key used is translated.** `t("…")` in `main.ts` / `render.ts` and
   `data-i18n*` in `index.html` must appear in `i18n.ts`'s DICT.
2. **Every key translated is used.** A leftover entry is a string nobody sees any more —
   either the call site was deleted or it was renamed, and both leave the message table
   lying about what the page says.
3. **Placeholders agree.** Chinese `{n} 分钟前` and English `{n} min ago` must use the same
   `{…}` names, or the value silently disappears from one language.

Run from the repository root: `python3 scripts/check_i18n.py`.
"""

from __future__ import annotations

import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SRC = ROOT / "joy-ts/packages/dashboard/src"

DICT_LINE = re.compile(r'^\s*"((?:[^"\\]|\\.)*)":\s*"((?:[^"\\]|\\.)*)",\s*$', re.M)
CALL = re.compile(r'\bt\(\s*"((?:[^"\\]|\\.)*)"')
DATA_ATTR = re.compile(r'data-i18n(?:-title|-placeholder|-aria)?="([^"]+)"')
PLACEHOLDER = re.compile(r"\{(\w+)\}")

problems: list[str] = []


def unquote(text: str) -> str:
    return json.loads('"' + text + '"')


def main() -> int:
    dict_src = (SRC / "i18n.ts").read_text(encoding="utf-8")
    dict_start = dict_src.index("const DICT: Record<string, string> = {")
    dict_end = dict_src.index("\n};", dict_start)
    entries = {
        unquote(zh): unquote(en)
        for zh, en in DICT_LINE.findall(dict_src[dict_start:dict_end])
    }
    if len(entries) < 100:
        problems.append(
            f"only {len(entries)} translations parsed — fix this script rather than leaving it green"
        )
        report()
        return 1

    # Every .ts under src/ except the dictionary itself: a `t()` call in login.ts is as much a
    # call site as one in render.ts, and the dictionary entry it needs is as mandatory.
    used: dict[str, str] = {}
    for path in sorted(SRC.glob("*.ts")):
        if path.name == "i18n.ts":
            continue
        for key in CALL.findall(path.read_text(encoding="utf-8")):
            used.setdefault(unquote(key), path.name)
    html = (SRC / "index.html").read_text(encoding="utf-8")
    for key in DATA_ATTR.findall(html):
        used.setdefault(key, "index.html")

    for key, where in sorted(used.items()):
        if key not in entries:
            problems.append(f"{where} uses text with no translation: {key}")
    for key in sorted(entries):
        if key not in used:
            problems.append(f"i18n.ts carries a translation nobody uses: {key}")

    for zh, en in entries.items():
        if PLACEHOLDER.findall(zh) != PLACEHOLDER.findall(en):
            problems.append(
                f"placeholders do not match: {zh} → {en}"
                f"（{sorted(PLACEHOLDER.findall(zh))} vs {sorted(PLACEHOLDER.findall(en))}）"
            )

    report(len(entries), len(used))
    return 1 if problems else 0


def report(translated: int = 0, used: int = 0) -> None:
    if problems:
        for problem in problems:
            print(f"✗ {problem}")
        print(f"\n{len(problems)} things to fix")
        return
    print(f"✓ UI text is bilingual: {translated} translations, {used} call sites")


if __name__ == "__main__":
    sys.exit(main())
