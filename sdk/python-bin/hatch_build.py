"""Put the compiled `joy` into the wheel, and give the wheel a real platform tag.

Without both, what comes out is a **broken wheel**: hatchling tags a pure-Python package as
`py3-none-any` ("installs anywhere"), and inside lies a binary that can only exec on one
architecture.

**Compiling is not this file's job.** Packaging should not quietly run cargo (a different
task at a different cost); it only finds one that is already built. For a single command
that does it all, use `just build-python-bin`.
"""

from __future__ import annotations

import os
import platform
import sys
import sysconfig
from pathlib import Path

from hatchling.builders.hooks.plugin.interface import BuildHookInterface

_HERE = Path(__file__).resolve().parent

#: Where it lands inside the wheel. `joyczl_agent_bin.binary()` looks along this path.
_IN_WHEEL = "joyczl_agent_bin/bin/joy"

#: The minimum OS version cargo sets per architecture on macOS. Why this rather than Python,
#: see below.
_MACOS_MINIMUM = {"arm64": (11, 0), "x86_64": (10, 12)}


def _platform_tag() -> str:
    """The wheel's platform tag.

    `sysconfig.get_platform()` is deliberately **not** used here, even though it looks exactly
    right. It describes the deployment target the Python interpreter was compiled for (on
    this machine `macosx-12_1-arm64`), which has nothing to do with the Rust binary inside
    the wheel; worse, it carries a non-zero minor — pip / uv only enumerate
    `macosx_<major>_0_<arch>` when judging compatibility, so `macosx_12_1_arm64` is refused
    **on its own machine**. And `uv build` says nothing about it; it only blows up when
    installed into a clean environment.

    The wheel holds a cargo-built binary, so its minimum OS version is Rust's target: 11.0 for
    arm64, 10.12 for x86_64 — the same arithmetic maturin does.

    Half of it is out of scope: on Linux a tag like `linux_x86_64` installs locally but cannot
    go to PyPI (that needs manylinux / auditwheel). That is a release-time job, for when
    there is a release.
    """
    arch = platform.machine()
    if sys.platform != "darwin":
        # Other systems have no such bend; what sysconfig gives is right (linux-x86_64 /
        # win-amd64).
        return sysconfig.get_platform().replace("-", "_").replace(".", "_")

    major, minor = _MACOS_MINIMUM.get(arch, (11, 0))
    return f"macosx_{major}_{minor}_{arch}"


def _find_binary() -> Path | None:
    """Find a compiled `joy`.

    release preferred: that is what goes out. Fall back to debug — when checking the packaging
    chain locally, that is the only one most people have, from `cargo build`.

    `$JOY_BIN` wins, the same rule the client uses at runtime.
    """
    override = os.environ.get("JOY_BIN")
    if override:
        candidate = Path(override)
        return candidate if candidate.is_file() else None

    for directory in _HERE.parents:
        for profile in ("release", "debug"):
            candidate = directory / "joy-rs" / "target" / profile / "joy"
            if candidate.is_file():
                return candidate
    return None


class PlatformTagHook(BuildHookInterface):
    def initialize(self, version: str, build_data: dict) -> None:
        binary = _find_binary()
        if binary is None:
            # **No error** when there is no binary; fall back to a pure-Python wheel: the
            # editable install (`uv sync --extra bin` — local development, and any machine
            # without a Rust build) comes through here, and so does producing a wheel — the
            # build_data handed in is identical, so the hook cannot tell them apart.
            #
            # So "the wheel that goes out must contain a binary" is not guarded here but at the
            # packaging entry point, `just build-python-bin`, which opens the box afterwards.
            self.app.display_warning(
                "no compiled `joy` found: this wheel will carry no binary (pure Python, any tag)."
                "For a wheel with the binary: run `cargo build --release` first, or point JOY_BIN at it."
            )
            return

        self.app.display_info(f"packing {binary} into the wheel")
        build_data["pure_python"] = False
        build_data["tag"] = f"py3-none-{_platform_tag()}"
        # The binary goes straight into the wheel, never into the source tree — so `bin/` in
        # the repo is always empty, and there is no "forgot to clean up after packaging, tens
        # of MB went into git" either.
        build_data["force_include"] = {str(binary): _IN_WHEEL}
