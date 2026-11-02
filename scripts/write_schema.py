#!/usr/bin/env python3
"""从 Rust 协议定义重新生成 TypeScript 与 JSON Schema。

    python3 scripts/write_schema.py            # 生成（并排版）
    python3 scripts/write_schema.py --check    # 只检查是否过期，不写文件（CI 用）
    python3 scripts/write_schema.py --python   # 额外生成 Python SDK 的生成物

为什么是 Python 驱动 `cargo test`：
  生成逻辑（ts-rs / schemars）只在 test 构建下编译 —— 非 test 构建里
  `#[derive(TS, JsonSchema)]` 被 joyczl-protocol-noop-macros 替换成空宏，
  所以线上协议 crate 不依赖 ts-rs / schemars，也不付任何 derive 成本。
  要触发生成就必须跑一次测试构建，这就是本文件存在的理由。


为什么排版和漂移比对也在这里、而不是在 Rust 里：
  prettier 只能作用于生成之后的文件。若在 Rust 的测试里比对，就是拿
  「刚生成的未排版产物」去比「仓库里已排版的提交物」，永远不相等。
  所以分工是：Rust 只生成，Python 负责排版 + 比对。
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

# Python SDK 的两份生成物。v2.py 是 63 个类型，consts.py 是两组常量。
PYTHON_SDK = REPO_ROOT / "sdk" / "python"
PYTHON_GENERATED = PYTHON_SDK / "src" / "joyczl_agent" / "generated"

SEPARATOR = "─" * 60


def run_cargo_export(out_dir: Path, cargo_args: list[str]) -> int:
    """跑协议导出的 ignored 测试，产物写到 out_dir。"""
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


def format_typescript(ts_dir: Path) -> None:
    """用 prettier 排版生成物。

    ts-rs 不开 `format` feature（会拖进与 serde 1.0.229 不兼容的 swc_common），
    所以排版交给 node 的 prettier。
    排不出来只影响 diff 观感，不影响正确性，所以失败只警告。
    """
    if not (ts_dir / "v2").is_dir():
        return
    # `.mts` 也要排 —— 常量文件（codes / methods）是 `.mts`，见 export.rs 的 write_consts。
    cmd = ["npx", "--yes", "prettier@3", "--write", str(ts_dir / "v2" / "*.{ts,mts}")]
    print("›", " ".join(cmd))
    if subprocess.run(cmd, cwd=REPO_ROOT).returncode != 0:
        print("⚠ prettier 不可用，生成物保持未排版（不影响正确性）", file=sys.stderr)


def snapshot(dirpath: Path) -> dict[str, str]:
    """目录快照：相对路径 → 内容。用于比对生成物是否漂移。"""
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
            problems.append(f"  新增（未提交）：{name}")
        elif name not in fresh:
            problems.append(f"  丢失（已提交但不再生成）：{name}")
        elif committed[name] != fresh[name]:
            problems.append(f"  内容不一致：{name}")
    return problems


def render_python_consts(consts: dict) -> str:
    """把 `json/consts.json` 渲染成 `generated/consts.py`。

    用枚举而不是两个 dict：`Methods.MEMORY_SEARCH` 打错字当场就是
    AttributeError + 编辑器能补全，而 `METHODS["MEMORY_SEARCH"]` 要等到
    真发出去才在服务端变成一句 METHOD_NOT_FOUND。

    纯字符串拼接、不碰任何外部工具 —— 所以 `--check` 里也能走同一条路重算
    一遍来比对（v2.py 做不到，它要 datamodel-codegen）。
    """

    def members(table: dict, *, quote: bool) -> str:
        return "\n".join(
            f'    {name} = "{value}"' if quote else f"    {name} = {value}"
            for name, value in sorted(table.items())
        )

    return (
        '"""协议里的两组常量。**生成物，别手改。**\n'
        "\n"
        "由 `scripts/write_schema.py` 从 `schema/json/consts.json` 生成，而那份 JSON\n"
        "由 Rust 的 `joyczl-protocol`（`rpc::codes`、`protocol::v2::methods`）导出。\n"
        "\n"
        "TS 侧同一份东西在 `schema/typescript/v2/{codes,methods}.mts` —— 同一个源头。\n"
        "**所以「memory/remember」这个字符串全仓库只有一处定义**，三种语言都从它生成。\n"
        '"""\n'
        "\n"
        "from __future__ import annotations\n"
        "\n"
        "from enum import Enum, IntEnum\n"
        "\n"
        "\n"
        "class Codes(IntEnum):\n"
        '    """错误码。名字跟 Rust 侧常量同名，好让两边 grep 得到同一处。"""\n'
        "\n"
        f"{members(consts['codes'], quote=False)}\n"
        "\n"
        "\n"
        "class Methods(str, Enum):\n"
        '    """方法名。值就是线上传的字符串。"""\n'
        "\n"
        "    # 3.10 没有 StrEnum；`str, Enum` 这手在 f-string 里会打出\n"
        '    # "Methods.TURN_START"，而方法名是要拼进日志和报错里的 —— 换回来。\n'
        "    __str__ = str.__str__\n"
        "\n"
        f"{members(consts['methods'], quote=True)}\n"
    )


def generate_python_models() -> int:
    """生成 Python SDK 的两份生成物。

    常量先来：它不依赖任何外部工具，所以哪怕 datamodel-codegen 因为没网、
    没装 uv 跑不起来，这一半也该是新的。
    """
    out = PYTHON_GENERATED / "v2.py"
    schema = GENERATED_ROOT / "json" / "v2.json"
    consts = GENERATED_ROOT / "json" / "consts.json"

    if not schema.exists() or not consts.exists():
        print(f"✗ 找不到 {schema} 或 {consts}，先跑一次生成", file=sys.stderr)
        return 1
    if not PYTHON_SDK.exists():
        print(f"✗ 还没有 {PYTHON_SDK}", file=sys.stderr)
        return 1

    PYTHON_GENERATED.mkdir(parents=True, exist_ok=True)
    consts_py = PYTHON_GENERATED / "consts.py"
    consts_py.write_text(
        render_python_consts(json.loads(consts.read_text(encoding="utf-8"))),
        encoding="utf-8",
    )
    print(f"✓ 常量 → {consts_py.relative_to(REPO_ROOT)}")

    cmd = [
        "uv", "run", "--project", str(PYTHON_SDK), "datamodel-codegen",
        "--input", str(schema),
        "--input-file-type", "jsonschema",
        "--output", str(out),
        "--output-model-type", "pydantic_v2.BaseModel",
        "--target-python-version", "3.10",
        # 判别式要变成 Literal 而不是一堆无名枚举：`n.type == "textDelta"` 能直接写。
        # 是 `one` 不是 `all` —— `all` 会把 `MessageRole` 这种具名多成员枚举也压成
        # `RootModel[Literal[...]]`，于是用户得写 `msg.role.root`。`one` 只动单成员
        # 枚举，而通知的 `type` 恰好每个都是单成员。
        "--enum-field-as-literal", "one",
        # 让 `MessageRole.user == "user"` 成立，而不是必须 `.value`。
        "--use-subclass-enum",
        # 默认会在文件头写一行生成时间 —— 那会让每次重生成都产生 diff，
        # 而这个文件是提交进仓库的。
        "--disable-timestamp",
        # 不带这个，带约束的字段会生成成 `limit: conint(ge=0) | None` ——
        # 把一个**函数调用**当类型标注写。pydantic 运行时能吞，mypy 直接判
        # 「Cannot use a function call in a type annotation」，于是一个 SDK 的
        # 生成物自己先过不了类型检查。带上就是
        # `Annotated[int | None, Field(ge=0)]`，两边都认。
        "--use-annotated",
    ]
    print("›", " ".join(cmd))
    return subprocess.run(cmd, cwd=REPO_ROOT).returncode


def main() -> int:
    parser = argparse.ArgumentParser(
        description="从 Rust 协议定义生成 TypeScript 与 JSON Schema")
    parser.add_argument("--check", action="store_true",
                        help="只检查生成物是否最新，不写文件（CI 用）")
    parser.add_argument("--python", action="store_true",
                        help="额外生成 Python pydantic 模型")
    parser.add_argument("cargo_args", nargs="*",
                        help="透传给 cargo test 的额外参数")
    args = parser.parse_args()

    scratch: str | None = None
    if args.check:
        scratch = tempfile.mkdtemp(prefix="joyczl-schema-")
        out_dir = Path(scratch)
    else:
        out_dir = GENERATED_ROOT

    try:
        rc = run_cargo_export(out_dir, list(args.cargo_args))
        if rc != 0:
            print("\n✗ 协议导出失败", file=sys.stderr)
            return rc

        # 排版必须在比对之前 —— 两边都排版过才可比。
        format_typescript(out_dir / "typescript")

        if args.check:
            print(SEPARATOR)
            committed_ts = snapshot(GENERATED_ROOT / "typescript")
            if not committed_ts:
                print("✗ 仓库里还没有生成物，先跑一次：just write-app-server-schema",
                      file=sys.stderr)
                return 1

            problems = diff_snapshots(committed_ts, snapshot(out_dir / "typescript"))

            # JSON 侧有两份：v2.json 是**类型**，consts.json 是**值**。
            for name in ("v2.json", "consts.json"):
                committed_json = GENERATED_ROOT / "json" / name
                fresh_json = out_dir / "json" / name
                committed_text = committed_json.read_text(encoding="utf-8") \
                    if committed_json.exists() else ""
                fresh_text = fresh_json.read_text(encoding="utf-8") \
                    if fresh_json.exists() else ""
                if committed_text != fresh_text:
                    problems.append(f"  内容不一致：json/{name}")

            # 再往下是 Python 侧。consts.py 不依赖外部工具，所以这里能白捡一个
            # 比对 —— 改了方法名却忘了重生成 Python，就在这一步红。
            # v2.py 要 datamodel-codegen，那个由 `--python` 自己保证。
            consts_py = PYTHON_GENERATED / "consts.py"
            if consts_py.exists():
                fresh_consts = json.loads(
                    (out_dir / "json" / "consts.json").read_text(encoding="utf-8"))
                if consts_py.read_text(encoding="utf-8") != render_python_consts(fresh_consts):
                    problems.append(
                        f"  内容不一致：{consts_py.relative_to(REPO_ROOT)}")

            if problems:
                print("✗ 生成物与 Rust 协议定义不一致：", file=sys.stderr)
                print("\n".join(problems), file=sys.stderr)
                print("\n跑一次：just write-app-server-schema", file=sys.stderr)
                return 1
            print("✓ 生成物与 Rust 协议定义一致")
            return 0

        print(SEPARATOR)
        print(f"✓ TypeScript → {GENERATED_ROOT / 'typescript'}")
        print(f"✓ JSON Schema → {GENERATED_ROOT / 'json' / 'v2.json'}")
        print(f"✓ 常量 → {GENERATED_ROOT / 'json' / 'consts.json'}")

        if args.python:
            return generate_python_models()
        return 0
    finally:
        if scratch:
            shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
