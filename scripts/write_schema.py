#!/usr/bin/env python3
"""Regenerate the TypeScript types and the JSON Schema from the Rust protocol.

    python3 scripts/write_schema.py            # generate (and format)
    python3 scripts/write_schema.py --check    # only check for staleness, write nothing (CI)

Why Python drives `cargo test`:
  The generation logic (ts-rs / schemars) is compiled only in a test build — outside one,
  `#[derive(TS, JsonSchema)]` is replaced by joyczl-protocol-noop-macros with an empty macro,
  so the shipped protocol crate does not depend on ts-rs / schemars and pays no derive cost.
  Triggering generation therefore means running a test build once, and that is the only reason
  this file exists.


Why formatting and the drift comparison live here rather than in Rust:
  prettier can only act on files that already exist. Comparing inside a Rust test would mean
  comparing "the unformatted artefact just produced" against "the formatted artefact committed
  to the repo" — never equal. So the split is: Rust generates, Python formats and compares.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
RUST_DIR = REPO_ROOT / "joy-rs"
PROTOCOL_CRATE = "joyczl-protocol"
GENERATED_ROOT = RUST_DIR / PROTOCOL_CRATE / "schema"
TEST_NAME = "export_tests::write_schema_fixtures"

# The two Python SDK artefacts: v2.py is 63 types, consts.py is the two constant tables.

SEPARATOR = "─" * 60


def run_cargo_export(out_dir: Path, cargo_args: list[str]) -> int:
    """Run the ignored export test; the artefacts land in out_dir."""
    env = os.environ.copy()
    env["JOYCZL_SCHEMA_ROOT"] = str(GENERATED_ROOT)
    env["JOYCZL_SCHEMA_OUT"] = str(out_dir)

    cmd = [
        "cargo", "test",
        "-p", PROTOCOL_CRATE,
        "--lib",
        TEST_NAME,
        "--", "--exact", "--ignored", "--nocapture",
        *cargo_args,
    ]
    print("›", " ".join(cmd))
    return subprocess.run(cmd, cwd=RUST_DIR, env=env).returncode


def format_typescript(ts_dir: Path) -> bool:
    """Format the artefacts with prettier; returns whether it actually happened.

    ts-rs does not enable the `format` feature (it drags in swc_common, which cannot coexist
    with serde 1.0.229), so formatting is node's prettier.
    Not being able to format only affects how the diff reads, not correctness, so failure is a
    warning.

    The return value matters in `--check`: without formatting, the scratch artefacts cannot be
    compared against the formatted ones in the repo — doing so reports a pile of "content
    differs" that is not real drift. A missing command has to take the same warning path:
    "failure only warns" has to catch `FileNotFoundError` too.
    """
    if not (ts_dir / "v2").is_dir():
        return False
    # `.mts` needs formatting too — the constant files (codes / methods) are `.mts`, see
    # write_consts in export.rs.
    cmd = ["npx", "--yes", "prettier@3", "--write", str(ts_dir / "v2" / "*.{ts,mts}")]
    print("›", " ".join(cmd))
    try:
        ok = subprocess.run(cmd, cwd=REPO_ROOT).returncode == 0
    except FileNotFoundError:
        print("⚠ npx not found; skipping formatting this time", file=sys.stderr)
        return False
    if not ok:
        print("⚠ prettier unavailable; artefacts stay unformatted (correctness unaffected)",
              file=sys.stderr)
    return ok


def snapshot(dirpath: Path) -> dict[str, str]:
    """A snapshot of a directory: relative path → content. Used for drift comparison."""
    out: dict[str, str] = {}
    if not dirpath.is_dir():
        return out
    for path in sorted(dirpath.rglob("*")):
        if path.is_file():
            out[str(path.relative_to(dirpath))] = path.read_text(encoding="utf-8")
    return out


def diff_snapshots(committed: dict[str, str], fresh: dict[str, str]) -> list[str]:
    problems = []
    for name in sorted(set(committed) | set(fresh)):
        if name not in committed:
            problems.append(f"  new (not committed): {name}")
        elif name not in fresh:
            problems.append(f"  gone (committed but no longer generated): {name}")
        elif committed[name] != fresh[name]:
            problems.append(f"  content differs: {name}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Generate the TypeScript types and JSON Schema from the Rust protocol")
    parser.add_argument("--check", action="store_true",
                        help="only check whether the artefacts are up to date; write nothing (CI)")
    parser.add_argument("cargo_args", nargs="*",
                        help="extra arguments for the cargo run that exports the schema")
    args = parser.parse_args()

    # Both modes generate into a scratch directory first: write mode has to know "which files
    # came out this time" before it can delete the orphans the repo still carries — ts-rs only
    # writes, never deletes, so removing a type from the protocol leaves the old file behind,
    # padding the repo (`--check` reports it as "committed but no longer generated").
    scratch = tempfile.mkdtemp(prefix="joyczl-schema-")
    out_dir = Path(scratch)

    try:
        rc = run_cargo_export(out_dir, list(args.cargo_args))
        if rc != 0:
            print("\n✗ the protocol export failed", file=sys.stderr)
            return rc

        # Formatting has to happen before the comparison — only two formatted trees compare.
        formatted = format_typescript(out_dir / "typescript")

        if args.check:
            print(SEPARATOR)
            committed_ts = snapshot(GENERATED_ROOT / "typescript")
            if not committed_ts:
                print("✗ the repo has no artefacts yet; run once: just write-app-server-schema",
                      file=sys.stderr)
                return 1

            if formatted:
                problems = diff_snapshots(committed_ts, snapshot(out_dir / "typescript"))
            else:
                # Unformatted means "drift" cannot be told apart from "not formatted" — a pile
                # of false differences is worse than none. JSON and Python do not need prettier,
                # so those are still compared.
                print("⚠ no usable prettier: skipping the TypeScript formatting comparison "
                      "(JSON / Python are still compared)",
                      file=sys.stderr)
                problems = []

            # Two files on the JSON side: v2.json is the **types**, consts.json the **values**.
            for name in ("v2.json", "consts.json"):
                committed_json = GENERATED_ROOT / "json" / name
                fresh_json = out_dir / "json" / name
                committed_text = committed_json.read_text(encoding="utf-8") \
                    if committed_json.exists() else ""
                fresh_text = fresh_json.read_text(encoding="utf-8") \
                    if fresh_json.exists() else ""
                if committed_text != fresh_text:
                    problems.append(f"  content differs: json/{name}")

            if problems:
                print("✗ the artefacts do not match the Rust protocol:", file=sys.stderr)
                print("\n".join(problems), file=sys.stderr)
                print("\nRun once: just write-app-server-schema", file=sys.stderr)
                return 1
            print("✓ the artefacts match the Rust protocol")
            return 0

        # Sync the repo to what came out this time: anything not produced is an orphan and goes,
        # anything produced is overwritten.
        committed = snapshot(GENERATED_ROOT / "typescript")
        fresh = snapshot(out_dir / "typescript")
        for rel in sorted(set(committed) - set(fresh)):
            (GENERATED_ROOT / "typescript" / rel).unlink()
            print(f"✓ removed the orphaned artefact typescript/{rel}")
        for rel in sorted(fresh):
            target = GENERATED_ROOT / "typescript" / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(out_dir / "typescript" / rel, target)
        for name in ("v2.json", "consts.json"):
            shutil.copyfile(out_dir / "json" / name, GENERATED_ROOT / "json" / name)

        print(SEPARATOR)
        print(f"✓ TypeScript → {GENERATED_ROOT / 'typescript'}")
        print(f"✓ JSON Schema → {GENERATED_ROOT / 'json' / 'v2.json'}")
        print(f"✓ constants → {GENERATED_ROOT / 'json' / 'consts.json'}")

        return 0
    finally:
        if scratch:
            shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
