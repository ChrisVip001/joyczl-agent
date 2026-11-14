#!/usr/bin/env python3
"""Config-doc reconciliation: every switch the code reads, the docs must mention.

    python3 scripts/check_config_docs.py      # for CI (prints ✓ or exits 1)

A `JOY_*` used to have to appear in four places at once (`.env.example`, both `configuration`
docs, and the read in the code), and human memory guarantees a miss. So **the code is the only
source of truth** and the docs are reconciled against it. Each of the four directions stands
for a real accident:

  1. the code reads it, `.env.example` does not list it — whoever copies the sample cannot
     switch it on;
  2. the code reads it, the `configuration` docs never mention it — the reader thinks the
     switch does not exist;
  3. `.env.example` lists it, the code never reads it — a dead switch looks usable;
  4. the docs mention it, the code never reads it — a leftover note after a rename is worse
     than nothing.

"Reads" is not only Rust: the Python SDK, the TS client and `scripts/*.sh` read the environment
too (`JOY_BIN` is the SDK's), and leaving one side out would mistake a live switch for a
leftover.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ENV_SAMPLE = ROOT / ".env.example"
DOCS = (ROOT / "docs" / "configuration.md", ROOT / "docs" / "configuration.zh.md")

# Readers that pass the name to env::var as **data**; the regexes cannot see those, so they are
# named here. The test: the line has no `env("X")` literal, yet the name is really read at
# runtime.
INDIRECT = {
    "JOY_SEARCH_API_KEY": "web.rs loops env::var over [\"TAVILY_API_KEY\", \"JOY_SEARCH_API_KEY\"]",
    "JOY_HOOK_SECRET": "a webhooks.json secret may carry an `env:` prefix, parsed by ops/webhooks.rs",
}

# Those are the only shapes that really "read" a variable. A name in a comment, in an error
# message or on the left of an assignment in a script does not count — `JOY_BASE_URL` was
# recommended inside error text for years (it has long since stopped being read).
READER_RES = (
    re.compile(r'\benv\w*\s*\(\s*"(JOY_[A-Z0-9_]+)"'),           # env / env_int / env_bool / env_opt
    re.compile(r'\benv::var(?:_os)?\s*\(\s*"(JOY_[A-Z0-9_]+)"'),  # std::env::var
    re.compile(r'\bvar_os\s*\(\s*"(JOY_[A-Z0-9_]+)"'),
    re.compile(r'\benv_name:\s*"(JOY_[A-Z0-9_]+)"'),               # the BOUNDS table
)

# How a script or SDK reads one: `os.environ.get("X")`, `process.env.X`, `${X}`, `getenv("X")`.
SCRIPT_READ = ("environ", "getenv", "process.env", "${")
WORD_RE = re.compile(r"\bJOY_[A-Z0-9_]+\b")

# Deliberately undocumented: each entry needs a reason, or this becomes a pocket of quiet
# exceptions. The test is "an ordinary user has no reason to know" — test fixtures, and the
# fake credentials the smoke scripts set for themselves.
INTERNAL = {
    "JOY_CONFIG_TEST_BOOL_XYZ": "a fake variable in the config tests, only there to prove bool parsing",
    "JOY_MODEL": "the tests clear it to exercise the \"no model configured\" branch",
    "JOY_FAKE_KEY": "test fixture: a fake key",
    "JOY_TEST_MODELS_KEY": "test fixture: a fake multi-model catalog",
    "JOY_FEISHU_SMOKE_SECRET": "the smoke script's own fixture credential",
    "JOY_IMAP_SMOKE_PASSWORD": "the smoke script's own fixture credential",
    # Set and read by the smoke run itself: it feeds the path of the TS module under test to node.
    "JOY_BRIDGE_URL": "smoke-gateway points at the bridge module under test",
    "JOY_LARK_URL": "smoke-gateway points at the lark module under test",
    "JOY_LARK_PROTO_URL": "smoke-gateway points at the lark-proto module under test",
    "JOY_WECHAT_URL": "smoke-gateway points at the wechat module under test",
}

# Below this count the extraction has stopped working (the code was rewritten, a directory
# moved); that has to be an error rather than a quiet pass — a reconciliation that is always
# green is worse than none.
MIN_VARS = 60

def fail(message: str) -> None:
    print(f"✗ {message}", file=sys.stderr)
    raise SystemExit(1)

def code_vars() -> set[str]:
    """Everywhere a `JOY_*` is **readable**: non-comment Rust literals plus the SDK/TS/scripts."""
    found: set[str] = set()

    for path in (ROOT / "joy-rs").rglob("*.rs"):
        if "target" in path.parts or path.name.endswith("_tests.rs"):
            continue
        for line in path.read_text(encoding="utf-8").splitlines():
            if line.strip().startswith("//"):
                continue
            for reader in READER_RES:
                found |= set(reader.findall(line))

    # Python SDK / scripts / TS client: they read the runtime environment in their own shapes,
    # so only the names are recognised.
    sources = [
        *(ROOT / "sdk").rglob("*.py"),
        *(ROOT / "scripts").glob("*.sh"),
        *(ROOT / "scripts").glob("*.py"),
        *(ROOT / "joy-ts" / "packages").rglob("*.ts"),
    ]
    for path in sources:
        if {"node_modules", "dist"} & set(path.parts) or path.name == Path(__file__).name:
            continue
        for line in path.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if stripped.startswith("#") or not any(k in line for k in SCRIPT_READ):
                continue
            found |= set(WORD_RE.findall(line))

    return found

def sample_vars() -> set[str]:
    """The names in `.env.example`: commented-out ones count (the convention is all of them
    commented, uncomment what you need)."""
    return set(
        re.findall(r"^#?\s*(JOY_[A-Z0-9_]+)=", ENV_SAMPLE.read_text(encoding="utf-8"), re.M)
    )

def main() -> int:
    code = (code_vars() | INDIRECT.keys()) - INTERNAL.keys()
    sample = sample_vars()
    doc = {path: set(WORD_RE.findall(path.read_text(encoding="utf-8"))) for path in DOCS}

    if len(code) < MIN_VARS:
        fail(
            f"only {len(code)} JOY_* parsed out of the code (expected ≥{MIN_VARS})"
            " — the extraction probably broke (an env helper was renamed, a directory moved); "
            "fix this script first"
        )

    if missing := sorted(code - sample):
        fail(
            f"{ENV_SAMPLE.name} is missing {len(missing)} switches: {', '.join(missing)}\n"
            "  whoever copies the sample cannot turn them on. Either add them (with a line of\n"
            "  comment), or show they only serve the tests and list them in INTERNAL with a reason"
        )

    for path, mentions in doc.items():
        if missing := sorted(code - mentions):
            fail(
                f"{path.name} never mentions {len(missing)} switches: {', '.join(missing)}\n"
                "  the reader will think those switches do not exist"
            )

    for name, mentions in [(".env.example", sample), *((p.name, m) for p, m in doc.items())]:
        if stale := sorted(mentions - code - INTERNAL.keys()):
            fail(
                f"{name} mentions {len(stale)} switches nobody reads: {', '.join(stale)}\n"
                "  a dead switch is worse than no switch: it looks usable"
            )

    print(
        f"✓ config docs reconcile: {len(code)} switches, "
        f"matching {ENV_SAMPLE.name}, {DOCS[0].name} and {DOCS[1].name} "
        f"({len(INTERNAL)} test fixtures deliberately left out)"
    )
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
