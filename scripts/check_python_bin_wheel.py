"""开箱确认 `joyczl-agent-bin` 打出来的轮子里真有一个二进制。

**为什么值得单独一步**：没找到二进制时构建并不会失败 —— hatchling 会安静地打出
一个 `py3-none-any` 的纯 Python 轮子。这不是能顺手修掉的东西：editable 装法
（`uv sync --extra bin`，本地开发和没编过 Rust 的机器都会走）跟出轮子传给 hook 的
`build_data` **一模一样**，hook 里分不出来，所以在那边报错会打断开发流程。

代价就是「打包时忘了编二进制」会安静地坏掉，而这个坏法很难看：轮子发出去、用户
`pip install "joyczl-agent[bin]"` 装得上、`pip` 一声不吭，直到运行才「找不到 joy」。
所以把检查放在打包入口：发出去之前开箱看一眼。

用法：check_python_bin_wheel.py [dist 目录，默认 dist]
"""

from __future__ import annotations

import sys
import zipfile
from pathlib import Path

#: 必须出现在轮子里的那个路径。跟 `hatch_build.py` 的 `_IN_WHEEL` 是同一处约定。
IN_WHEEL = "joyczl_agent_bin/bin/joy"


def main(argv: list[str]) -> int:
    dist = Path(argv[1] if len(argv) > 1 else "dist")
    wheels = sorted(dist.glob("*.whl"))
    if not wheels:
        print(f"✗ {dist} 里没有轮子")
        return 1

    # 取最新那个：`uv build` 不会清 dist，多平台构建时这里会有好几个。
    wheel = wheels[-1]
    with zipfile.ZipFile(wheel) as archive:
        names = archive.namelist()
        size = archive.getinfo(IN_WHEEL).file_size if IN_WHEEL in names else 0

    problems = []
    if wheel.name.endswith("-any.whl"):
        problems.append("标签是 any（纯 Python）—— 里面不可能有二进制")
    if IN_WHEEL not in names:
        problems.append(f"轮子里没有 {IN_WHEEL}")
    if problems:
        print(f"✗ {wheel.name}")
        for problem in problems:
            print(f"    {problem}")
        print("  先 `cargo build --release`（在 joy-rs/ 下），或者用 JOY_BIN 指到二进制")
        return 1

    print(f"✓ {wheel.name}：里面有一个 {size / 1e6:.1f} MB 的 joy")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
