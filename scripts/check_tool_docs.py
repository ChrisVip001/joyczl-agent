#!/usr/bin/env python3
"""工具文档对账：代码里有几个工具，文档里就得写几个。

    python3 scripts/check_tool_docs.py        # CI 用（通过打印 ✓，不通过 exit 1）

同一个数字散在三处，它们必须同时成立：

  1. **代码** —— `handlers::build_default()` 装的那批。名单取自
     `default_registry_has_the_expected_tools` 里被断言过的 `registry.names()`：
     那个测试要求全等，所以这份名单不会悄悄漂。
  2. **`docs/internals/06-tools.md` §6.0** —— 清单本身，以及标题里的
     「N 个常驻 + M 个门控」。数字对不上比名字漏了更糟：读者会以为清单是全的。
  3. **`README.md` / `README.zh.md`** —— 面向用户的那张表。

门控工具（`run_command` 等）与常驻工具的区别只能从 `app-server/src/lib.rs` 读出来：
它们出现在 `tools.register(joyczl_tools::…::fn(…))` 里，而 `todo_write` 同样是这么注册的、
却是常驻。所以 `ALWAYS_EXTRA` 单独列出它，并注明理由 —— 与其去解析 `if` 的条件，
不如把这一条事实写明白。

为什么不是 cargo test：这几份文件不在 crate 里。打包出去的 crate 不该因为仓库里的
Markdown 换个写法就红，而这条检查要的正是「仓库整体」的一致性 —— 和
`check-app-server-schema` 同一个位置。
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOLS_TESTS = ROOT / "joy-rs" / "joyczl-tools" / "src" / "tools_tests.rs"
APP_SERVER = ROOT / "joy-rs" / "joyczl-app-server" / "src" / "lib.rs"
DOC = ROOT / "docs" / "internals" / "06-tools.md"
READMES = (ROOT / "README.md", ROOT / "README.zh.md")

TEST_NAME = "default_registry_has_the_expected_tools"

# `app-server` 里无条件注册、但不在 `build_default()` 里的那些。
# `todo_write` 常驻：它不碰权限、不碰存储（见 lib.rs 里那句 "present by default"）。
ALWAYS_EXTRA = {"todo_write"}

# 低于这个数说明抽取方式失效了（代码换了写法），此时必须报错而不是默默通过 ——
# 一条永远绿的对账比没有对账更糟。
MIN_ALWAYS = 15
MIN_GATED = 5

# 门控工具用 `register_exclusive` 注册（写/执行/子智能体：它们必须独占运行），
# 常驻的仍用 `register` —— 两种写法都要认。
GATED_RE = re.compile(
    r"tools\.register(?:_exclusive)?\(\s*joyczl_tools::(?:[a-z_]+::)?([a-z_0-9]+)\s*\("
)
HEADING_RE = re.compile(r"^## 6\.0 清单：(\d+) 个常驻 \+ (\d+) 个门控", re.M)

# README 里的工具表：只认这一节。整份文件里搜名字是没用的 —— `read_skill` 还会出现在
# 阶段表和时间线里，那样一条「表里漏了」的漂移永远不会被发现。
README_SECTION_RE = re.compile(r"^### (Built-in tools|内置工具)\s*$", re.M)


def fail(message: str) -> None:
    print(f"✗ {message}", file=sys.stderr)
    raise SystemExit(1)


def build_default_names() -> list[str]:
    """`registry.names()` —— 被 `default_registry_has_the_expected_tools` 钉住的名单。"""
    text = TOOLS_TESTS.read_text(encoding="utf-8")
    anchor = f"async fn {TEST_NAME}"
    if anchor not in text:
        fail(f"{TOOLS_TESTS} 里找不到 `{TEST_NAME}` —— 这条检查的名单来源没了，别让它静默通过")
    start = text.index(anchor)
    call = text.index("registry.names(),", start)
    end = text.index("]", call)
    names = sorted(set(re.findall(r'"([a-z_0-9]+)"', text[call:end])))
    if len(names) < MIN_ALWAYS - len(ALWAYS_EXTRA):
        fail(
            f"只从 {TEST_NAME} 里解析出 {len(names)} 个工具（预期 ≥{MIN_ALWAYS - len(ALWAYS_EXTRA)}）"
            " —— 那个测试大概换写法了，先修这个脚本再谈文档"
        )
    return names


def app_server_names() -> set[str]:
    """`app-server` 自己注册的那批：`todo_write` 常驻，其余全是门控。"""
    return set(GATED_RE.findall(APP_SERVER.read_text(encoding="utf-8")))


def readme_table(text: str, name: str) -> str:
    """`### Built-in tools` 到下一个 `### ` 之间的那段（就是工具表）。"""
    heading = README_SECTION_RE.search(text)
    if heading is None:
        fail(f"{name} 里找不到工具表的标题（`### Built-in tools` / `### 内置工具`）")
    start = heading.start()
    following = re.search(r"^### ", text[heading.end() :], re.M)
    end = heading.end() + following.start() if following else len(text)
    return text[start:end]


def section_6_0(doc: str) -> str:
    try:
        start = doc.index("## 6.0")
        end = doc.index("## 6.1")
    except ValueError:
        fail(f"{DOC} 里找不到 §6.0（清单）到 §6.1 这一段")
    return doc[start:end]


def main() -> int:
    always = set(build_default_names()) | ALWAYS_EXTRA
    registered = app_server_names()
    gated = registered - always

    if len(always) < MIN_ALWAYS or len(gated) < MIN_GATED:
        fail(
            f"抽取结果不合理：常驻 {len(always)}、门控 {len(gated)}"
            " —— 先确认 lib.rs 里的注册写法，别让这条检查空转"
        )

    doc = DOC.read_text(encoding="utf-8")
    section = section_6_0(doc)

    missing_doc = sorted(t for t in always | gated if f"`{t}`" not in section)
    if missing_doc:
        fail(
            f"{DOC} §6.0 的清单里漏了 {len(missing_doc)} 个工具：{', '.join(missing_doc)}\n"
            "  清单是给人看的目录，漏了就等于告诉读者「这个工具不存在」"
        )

    heading = HEADING_RE.search(doc)
    if heading is None:
        fail(f"{DOC} 的 §6.0 标题不再带数字（应为「N 个常驻 + M 个门控」）—— 数字是这条检查的一半")
    want = (len(always), len(gated))
    got = (int(heading.group(1)), int(heading.group(2)))
    if got != want:
        fail(
            f"{DOC} §6.0 标题说 {got[0]} 个常驻 + {got[1]} 个门控，代码里是 {want[0]} + {want[1]}"
        )

    for readme in READMES:
        table = readme_table(readme.read_text(encoding="utf-8"), readme.name)
        missing = sorted(t for t in always | gated if f"`{t}`" not in table)
        if missing:
            fail(f"{readme.name} 的工具表里漏了：{', '.join(missing)}")

    print(
        f"✓ 工具文档对账：{want[0]} 常驻 + {want[1]} 门控 = {want[0] + want[1]}，"
        f"与代码、{DOC.name}、两份 README 一致"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
