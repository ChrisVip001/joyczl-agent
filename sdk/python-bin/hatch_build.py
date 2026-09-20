"""把编译好的 `joy` 塞进轮子，并让轮子带上真实的平台标签。

两件事都不做的话，打出来的是个**坏轮子**：hatchling 默认给纯 Python 包打
`py3-none-any`（「哪台机器都能装」），而里面躺着一个只能在某个架构上 exec 的二进制。

**这里不负责编译**。打包不该偷偷跑一次 cargo（那是另一件事、另一段耗时），
它只管找一个已经编好的。真要一条命令走完，用 `just build-python-bin`。
"""

from __future__ import annotations

import os
import platform
import sys
import sysconfig
from pathlib import Path

from hatchling.builders.hooks.plugin.interface import BuildHookInterface

_HERE = Path(__file__).resolve().parent

#: 进轮子之后的位置。`joyczl_agent_bin.binary()` 就是照这个路径找的。
_IN_WHEEL = "joyczl_agent_bin/bin/joy"

#: macOS 上 cargo 给每种架构定的最低系统版本。为什么照这个而不照 Python，见下。
_MACOS_MINIMUM = {"arm64": (11, 0), "x86_64": (10, 12)}


def _platform_tag() -> str:
    """轮子的平台标签。

    这里特意**不用** `sysconfig.get_platform()`，尽管它看上去正合适。它说的是
    这个 Python 解释器编译时的部署目标（本机上给的是 `macosx-12_1-arm64`），跟轮子
    里那个 Rust 二进制没关系；更要命的是它带非零 minor —— pip / uv 判兼容时只枚举
    `macosx_<大版本>_0_<arch>` 这一串，于是 `macosx_12_1_arm64` 在**自己机器上**都会
    被拒装。而这个错 `uv build` 一声不吭，要装进干净环境才炸出来。

    轮子里是 cargo 编的二进制，它的最低系统版本由 Rust 的 target 决定：arm64 是
    11.0，x86_64 是 10.12 —— 跟 maturin 的算法一致。

    还有一半不管：Linux 上 `linux_x86_64` 这种标签本地装得上，但发不到 PyPI
    （那边要 manylinux / auditwheel 洗过的）。那是发布时的事，等真要发了再说。
    """
    arch = platform.machine()
    if sys.platform != "darwin":
        # 别的系统没这层弯子，sysconfig 给的就是对的（linux-x86_64 / win-amd64）。
        return sysconfig.get_platform().replace("-", "_").replace(".", "_")

    major, minor = _MACOS_MINIMUM.get(arch, (11, 0))
    return f"macosx_{major}_{minor}_{arch}"


def _find_binary() -> Path | None:
    """找个编译好的 `joy`。

    偏好 release：要发出去的是它。没有就退回 debug —— 本地验一遍打包链路时，
    绝大多数人手上只有 `cargo build` 出来的那个。

    `$JOY_BIN` 优先，跟客户端运行时同一套规矩。
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
            # 没二进制时**不报错**，退回纯 Python 轮子：editable 装法
            # （`uv sync --extra bin`，本地开发、以及没编过 Rust 的机器）会走到这，
            # 而出轮子时也走到这 —— 两者传进来的 build_data 一模一样，hook 里分不出来。
            #
            # 所以「发出去的轮子里必须有二进制」这条不在这里守，由打包入口
            # `just build-python-bin` 守：它打完会开箱检查。
            self.app.display_warning(
                "没找到编译好的 `joy`，这个轮子将不含二进制（纯 Python、any 标签）。"
                "要打带二进制的轮子：先 `cargo build --release`，或用 JOY_BIN 指到它。"
            )
            return

        self.app.display_info(f"把 {binary} 打进轮子")
        build_data["pure_python"] = False
        build_data["tag"] = f"py3-none-{_platform_tag()}"
        # 二进制直接进轮子，不落到源码树里 —— 仓库里的 `bin/` 因此永远是空的，
        # 也就没有「打完包忘了清理、几十 MB 进了 git」这回事。
        build_data["force_include"] = {str(binary): _IN_WHEEL}
