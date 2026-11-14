"""Open the box and confirm the wheel `joyczl-agent-bin` produced really holds a binary.

**Why this is worth a step of its own**: a missing binary does not fail the build — hatchling
quietly produces a pure-Python `py3-none-any` wheel. And it cannot simply be fixed in the hook:
an editable install (`uv sync --extra bin`, which is what local development and any machine
without a Rust build take) hands the hook exactly the same `build_data` as a real build, so the
hook cannot tell them apart, and erroring there would break the development loop.

The price is that "forgot to build the binary before packaging" breaks quietly, and it breaks
ugly: the wheel ships, `pip install "joyczl-agent[bin]"` succeeds, pip says nothing, and only at
runtime does it turn into "cannot find joy". So the check goes at the packaging entry point: look
inside the box before it goes out.

Usage: check_python_bin_wheel.py [dist directory, dist by default]
"""

from __future__ import annotations

import sys
import zipfile
from pathlib import Path

#: The one path that must appear inside the wheel; the same agreement as `_IN_WHEEL` in
#: `hatch_build.py`.
IN_WHEEL = "joyczl_agent_bin/bin/joy"

def main(argv: list[str]) -> int:
    dist = Path(argv[1] if len(argv) > 1 else "dist")
    wheels = sorted(dist.glob("*.whl"))
    if not wheels:
        print(f"✗ {dist} holds no wheel")
        return 1

    # Take the newest one: `uv build` does not clear dist, so a multi-platform build leaves several.
    wheel = wheels[-1]
    with zipfile.ZipFile(wheel) as archive:
        names = archive.namelist()
        size = archive.getinfo(IN_WHEEL).file_size if IN_WHEEL in names else 0

    problems = []
    if wheel.name.endswith("-any.whl"):
        problems.append("the tag is any (pure Python) — a binary cannot be in there")
    if IN_WHEEL not in names:
        problems.append(f"the wheel does not carry {IN_WHEEL}")
    if problems:
        print(f"✗ {wheel.name}")
        for problem in problems:
            print(f"    {problem}")
        print("  run `cargo build --release` first (in joy-rs/), or point JOY_BIN at the binary")
        return 1

    print(f"✓ {wheel.name}: carries a {size / 1e6:.1f} MB joy")
    return 0

if __name__ == "__main__":
    sys.exit(main(sys.argv))
