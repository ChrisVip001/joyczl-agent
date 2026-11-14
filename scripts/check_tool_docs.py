#!/usr/bin/env python3
"""Tool-doc reconciliation: as many tools as the code has, the docs must list.

    python3 scripts/check_tool_docs.py        # for CI (prints ✓ or exits 1)

The same number is scattered across three places and all three have to hold:

  1. **The code** — what `handlers::build_default()` installs. The list is taken from the
     `registry.names()` asserted in `default_registry_has_the_expected_tools`: that test
     demands exact equality, so this list cannot drift quietly.
  2. **`docs/internals/06-tools.md` §6.0** — the inventory itself, plus the
     "N always on + M gated" in its heading. A wrong number is worse than a missing name:
     the reader would believe the inventory is complete.
  3. **`README.md` / `README.zh.md`** — the table aimed at users.

The difference between a gated tool (`run_command` and friends) and an always-on one can only
be read out of `app-server/src/server/*.rs`: they appear in `tools.register(joyczl_tools::…::fn(…))`,
and `todo_write` is registered the same way yet is always on. So `ALWAYS_EXTRA` lists it
separately and says why — writing that one fact down beats parsing the `if` conditions.

Why not cargo test: these files are not in a crate. A published crate should not go red because
a Markdown file in the repo was reworded, and what this check wants is consistency across the
**whole repo** — the same position as `check-app-server-schema`.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOLS_TESTS = ROOT / "joy-rs" / "joyczl-tools" / "src" / "tools_tests.rs"
APP_SERVER = ROOT / "joy-rs" / "joyczl-app-server" / "src"
DOC = ROOT / "docs" / "internals" / "06-tools.md"
READMES = (ROOT / "README.md", ROOT / "README.zh.md")

TEST_NAME = "default_registry_has_the_expected_tools"

# What `app-server` registers unconditionally but `build_default()` does not hold.
# `todo_write` is always on: it touches neither permissions nor storage (see the
# "present by default" line in server_runtime.rs).
ALWAYS_EXTRA = {"todo_write"}

# Below this count the extraction has stopped working (the code was rewritten); that has to be
# an error rather than a quiet pass — a reconciliation that is always green is worse than none.
MIN_ALWAYS = 15
MIN_GATED = 5

# Gated tools are registered with `register_exclusive` (write / exec / subagent: they must run
# exclusively), always-on ones still use `register` — both spellings have to be recognised.
GATED_RE = re.compile(
    r"tools\.register(?:_exclusive)?\(\s*joyczl_tools::(?:[a-z_]+::)?([a-z_0-9]+)\s*\("
)
# The heading of the Chinese doc set: "## 6.0 清单：N 个常驻 + M 个门控".
HEADING_RE = re.compile(r"^## 6\.0 清单：(\d+) 个常驻 \+ (\d+) 个门控", re.M)

# The tool table in a README: that section only. Searching the whole file for a name is
# useless — `read_skill` also shows up in the phase table and the timeline, and then a real
# "missing from the table" drift would never be found.
README_SECTION_RE = re.compile(r"^### (Built-in tools|内置工具)\s*$", re.M)

# The "N built-in tools (A always on, B gated)" sentence near the top of a README. Names
# complete but the total wrong is drift too — and harder to spot: the table looks whole and
# only that one number lies. One line per language, so a rewording makes this check report
# "the count sentence is missing" rather than skip silently.
COUNT_RE = re.compile(
    r"(\d+) built-in tools \((\d+) always on, (\d+) behind switches\)"
    r"|(\d+) 个内置工具（(\d+) 个默认常驻，(\d+) 个在开关后面）"
)


def fail(message: str) -> None:
    print(f"✗ {message}", file=sys.stderr)
    raise SystemExit(1)


def build_default_names() -> list[str]:
    """`registry.names()` — the list pinned by `default_registry_has_the_expected_tools`."""
    text = TOOLS_TESTS.read_text(encoding="utf-8")
    anchor = f"async fn {TEST_NAME}"
    if anchor not in text:
        fail(f"`{TEST_NAME}` is not in {TOOLS_TESTS} — this check's source of names is gone; "
             "do not let it pass silently")
    start = text.index(anchor)
    call = text.index("registry.names(),", start)
    end = text.index("]", call)
    names = sorted(set(re.findall(r'"([a-z_0-9]+)"', text[call:end])))
    if len(names) < MIN_ALWAYS - len(ALWAYS_EXTRA):
        fail(
            f"only {len(names)} tools parsed out of {TEST_NAME} "
            f"(expected ≥{MIN_ALWAYS - len(ALWAYS_EXTRA)})"
            " — that test was probably rewritten; fix this script before worrying about the docs"
        )
    return names


def app_server_names() -> set[str]:
    """What `app-server` registers itself: `todo_write` is always on, the rest are gated."""
    # Every module under server/ — registrations live wherever the server is assembled, and a
    # hard-coded pair of file names meant this check broke the day those files moved.
    text = "\n".join(
        path.read_text(encoding="utf-8") for path in sorted((APP_SERVER / "server").glob("*.rs"))
    )
    return set(GATED_RE.findall(text))


def readme_table(text: str, name: str) -> str:
    """The stretch from `### Built-in tools` to the next `### ` — that is the tool table."""
    heading = README_SECTION_RE.search(text)
    if heading is None:
        fail(f"{name} has no tool-table heading (`### Built-in tools` / `### 内置工具`)")
    start = heading.start()
    following = re.search(r"^### ", text[heading.end() :], re.M)
    end = heading.end() + following.start() if following else len(text)
    return text[start:end]


def section_6_0(doc: str) -> str:
    try:
        start = doc.index("## 6.0")
        end = doc.index("## 6.1")
    except ValueError:
        fail(f"{DOC} has no §6.0 (the inventory) through §6.1")
    return doc[start:end]


def main() -> int:
    always = set(build_default_names()) | ALWAYS_EXTRA
    registered = app_server_names()
    gated = registered - always

    if len(always) < MIN_ALWAYS or len(gated) < MIN_GATED:
        fail(
            f"the extraction looks wrong: {len(always)} always on, {len(gated)} gated"
            " — check how app-server registers them; do not let this check spin idle"
        )

    doc = DOC.read_text(encoding="utf-8")
    section = section_6_0(doc)

    missing_doc = sorted(t for t in always | gated if f"`{t}`" not in section)
    if missing_doc:
        fail(
            f"the §6.0 inventory in {DOC} is missing {len(missing_doc)} tools: "
            f"{', '.join(missing_doc)}\n"
            "  the inventory is a catalogue for a human; leaving one out tells the reader that "
            "tool does not exist"
        )

    heading = HEADING_RE.search(doc)
    if heading is None:
        fail(f"the §6.0 heading in {DOC} no longer carries the counts "
             "(it should read \"N 个常驻 + M 个门控\") — those numbers are half of this check")
    want = (len(always), len(gated))
    got = (int(heading.group(1)), int(heading.group(2)))
    if got != want:
        fail(
            f"the §6.0 heading in {DOC} says {got[0]} always on + {got[1]} gated, "
            f"the code has {want[0]} + {want[1]}"
        )

    counts = (want[0] + want[1], want[0], want[1])
    for readme in READMES:
        text = readme.read_text(encoding="utf-8")
        found = COUNT_RE.search(text)
        if found is None:
            fail(
                f"{readme.name} has no \"N built-in tools (A always on, B behind switches)\" "
                "sentence — that is the first number a reader sees; it cannot be missing"
            )
        said = tuple(int(g) for g in found.groups() if g is not None)
        if said != counts:
            fail(
                f"{readme.name} says {said[0]} built-in tools ({said[1]} always on + "
                f"{said[2]} gated), the code has {counts[0]} ({counts[1]} + {counts[2]})"
            )
        table = readme_table(text, readme.name)
        missing = sorted(t for t in always | gated if f"`{t}`" not in table)
        if missing:
            fail(f"the tool table in {readme.name} is missing: {', '.join(missing)}")

    print(
        f"✓ tool docs reconcile: {want[0]} always on + {want[1]} gated = {want[0] + want[1]}, "
        f"matching the code, {DOC.name} and both READMEs"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
