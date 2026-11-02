"""`joy` 二进制本体。

**为什么单独一个包**：二进制是分平台的，而客户端是纯 Python 的。混在一个包里，
`py3-none-any` 那个轮子就会被一个只能在某个架构上跑的 elf 绑死；拆开之后
`joyczl-agent` 保持纯 Python，装不装二进制由用户按平台选。

这个包里的 `bin/` 在仓库里**永远是空的** —— 打包时那个构建钩子（`hatch_build.py`）
把 `joy-rs/target/{release,debug}/joy` 直接放进轮子（`force_include`），不经过源码树。
所以也没有「打完包忘了清理、几十 MB 进了 git」这回事。要一条命令走完：
`just build-python-bin`（它会开箱确认二进制真在轮子里）。

本地开发时大家走 editable 装法，`bin/` 里没东西，`find_joy_binary()` 会自动往后
落到仓库的 target 目录。
"""

from __future__ import annotations

import sys
from pathlib import Path

_BIN = Path(__file__).resolve().parent / "bin"


def bin_dir() -> Path:
    """二进制所在的目录。"""
    return _BIN


def binary() -> Path:
    """`joy` 该在的位置。

    只报位置，不保证存在 —— 没编译过、或者本地 editable 装法下它就是个空目录。
    调用方按「这个文件在不在」判断，而不是按「这个包装没装」。
    """
    return _BIN / ("joy.exe" if sys.platform == "win32" else "joy")
