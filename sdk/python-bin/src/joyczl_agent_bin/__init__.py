"""The `joy` binary itself.

**Why a package of its own**: the binary is per-platform, the client is pure Python. In one
package the `py3-none-any` wheel would be nailed down by an elf that only runs on one
architecture; split apart, `joyczl-agent` stays pure Python and whether to take a binary is
the user's per-platform choice.

`bin/` in this package is **always empty in the repo** — at packaging time the build hook
(`hatch_build.py`) puts `joy-rs/target/{release,debug}/joy` straight into the wheel
(`force_include`), never through the source tree. So there is no "forgot to clean up after
packaging, tens of MB went into git" either. For one command that does it all:
`just build-python-bin` (which opens the box to confirm the binary really is in the wheel).

Local development uses the editable install, `bin/` holds nothing, and `find_joy_binary()`
falls back to the repo's target directory.
"""

from __future__ import annotations

import sys
from pathlib import Path

_BIN = Path(__file__).resolve().parent / "bin"


def bin_dir() -> Path:
    """The directory the binary lives in."""
    return _BIN


def binary() -> Path:
    """Where `joy` should be.

    It reports the location, not that it exists — never compiled, or under a local editable
    install, and this is an empty directory. Callers judge by "is this file there", not by
    "is this package installed".
    """
    return _BIN / ("joy.exe" if sys.platform == "win32" else "joy")
