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
            f"只解析出 {len(entries)} 条翻译 —— 先修这个脚本，别让它一直绿着"
        )
        report()
        return 1

    used: dict[str, str] = {}
    for name in ("main.ts", "render.ts"):
        text = (SRC / name).read_text(encoding="utf-8")
        for key in CALL.findall(text):
            used.setdefault(unquote(key), name)
    html = (SRC / "index.html").read_text(encoding="utf-8")
    for key in DATA_ATTR.findall(html):
        used.setdefault(key, "index.html")

    for key, where in sorted(used.items()):
        if key not in entries:
            problems.append(f"{where} 用了没翻译的文案：{key}")
    for key in sorted(entries):
        if key not in used:
            problems.append(f"i18n.ts 里有没人用的翻译：{key}")

    for zh, en in entries.items():
        if PLACEHOLDER.findall(zh) != PLACEHOLDER.findall(en):
            problems.append(
                f"占位符对不上：{zh} → {en}"
                f"（{sorted(PLACEHOLDER.findall(zh))} vs {sorted(PLACEHOLDER.findall(en))}）"
            )

    report(len(entries), len(used))
    return 1 if problems else 0


def report(translated: int = 0, used: int = 0) -> None:
    if problems:
        for problem in problems:
            print(f"✗ {problem}")
        print(f"\n{len(problems)} 处需要修")
        return
    print(f"✓ 界面文案双语对齐：{translated} 条翻译，{used} 个调用点")


if __name__ == "__main__":
    sys.exit(main())
