#!/usr/bin/env python3
"""配置文档对账：代码能读的每个开关，文档里都得有。

    python3 scripts/check_config_docs.py      # CI 用（通过打印 ✓，不通过 exit 1）

一个 `JOY_*` 原来要同时出现在四处（`.env.example`、两份 `configuration`、代码里那次读取），
靠人记就一定会漏。所以**代码是唯一事实源**，文档被对账。四个方向各对应一种真实事故：

  1. 代码能读、`.env.example` 里没有 —— 抄样例的人开不出这个功能；
  2. 代码能读、`configuration` 里没提 —— 读者以为开关不存在；
  3. `.env.example` 里有、代码读不到 —— 死开关看起来能用；
  4. 文档里提到、代码读不到 —— 改名后残留的说明比没有更糟。

"读得到"不止 Rust：Python SDK、TS 客户端、`scripts/*.sh` 也读环境变量（`JOY_BIN` 就是
SDK 读的），少算一方会把真开关误判成残文。
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ENV_SAMPLE = ROOT / ".env.example"
DOCS = (ROOT / "docs" / "configuration.md", ROOT / "docs" / "configuration.zh.md")

# 名字被当成**数据**传给 env::var 的那种读者，正则看不见，只能在这里点名。
# 判据：同一行里没有 `env("X")` 这种字面量，但运行时确实会去读它。
INDIRECT = {
    "JOY_SEARCH_API_KEY": "web.rs 在 [\"TAVILY_API_KEY\", \"JOY_SEARCH_API_KEY\"] 里循环 env::var",
    "JOY_HOOK_SECRET": "webhooks.json 的 secret 支持 `env:` 前缀，由 ops/webhooks.rs 解析",
}

# 真正「读」一个变量的写法只有这几种。名字出现在注释、错误提示或脚本里的赋值语句中
# 都不算 —— `JOY_BASE_URL` 就曾在错误文案里被推荐过好几年（它早就不读了）。
READER_RES = (
    re.compile(r'\benv\w*\s*\(\s*"(JOY_[A-Z0-9_]+)"'),           # env / env_int / env_bool / env_opt
    re.compile(r'\benv::var(?:_os)?\s*\(\s*"(JOY_[A-Z0-9_]+)"'),  # std::env::var
    re.compile(r'\bvar_os\s*\(\s*"(JOY_[A-Z0-9_]+)"'),
    re.compile(r'\benv_name:\s*"(JOY_[A-Z0-9_]+)"'),               # BOUNDS 表
)

# 脚本/SDK 的读法：`os.environ.get("X")`、`process.env.X`、`${X}`、`getenv("X")`。
SCRIPT_READ = ("environ", "getenv", "process.env", "${")
WORD_RE = re.compile(r"\bJOY_[A-Z0-9_]+\b")

# 刻意不写进文档的：每一条都要有理由，否则这就是个「悄悄放行」的口袋。
# 判据是「普通用户不需要知道」—— 测试夹具、smoke 脚本自己设的假凭证。
INTERNAL = {
    "JOY_CONFIG_TEST_BOOL_XYZ": "配置测试里的假变量，只用来证明 bool 解析",
    "JOY_MODEL": "测试里清空它，验证「没配模型」的分支",
    "JOY_FAKE_KEY": "测试夹具：一个假 key",
    "JOY_TEST_MODELS_KEY": "测试夹具：一份假的多模型清单",
    "JOY_FEISHU_SMOKE_SECRET": "smoke 脚本自己的夹具凭证",
    "JOY_IMAP_SMOKE_PASSWORD": "smoke 脚本自己的夹具凭证",
    # smoke 自己设、自己读：用来把「被测的那个 TS 模块」路径喂给 node。
    "JOY_BRIDGE_URL": "smoke-gateway 指向被测的 bridge 模块",
    "JOY_LARK_URL": "smoke-gateway 指向被测的 lark 模块",
    "JOY_LARK_PROTO_URL": "smoke-gateway 指向被测的 lark-proto 模块",
    "JOY_WECHAT_URL": "smoke-gateway 指向被测的 wechat 模块",
}

# 低于这个数说明抽取方式失效（改了写法 / 挪了目录），此时必须报错而不是默默通过：
# 一条永远绿的对账比没有对账更糟。
MIN_VARS = 60

def fail(message: str) -> None:
    print(f"✗ {message}", file=sys.stderr)
    raise SystemExit(1)

def code_vars() -> set[str]:
    """凡是**能读到** `JOY_*` 的地方：Rust 非注释字面量 + SDK/TS/脚本里的明文。"""
    found: set[str] = set()

    for path in (ROOT / "joy-rs").rglob("*.rs"):
        if "target" in path.parts or path.name.endswith("_tests.rs"):
            continue
        for line in path.read_text(encoding="utf-8").splitlines():
            if line.strip().startswith("//"):
                continue
            for reader in READER_RES:
                found |= set(reader.findall(line))

    # Python SDK / 脚本 / TS 客户端：它们读的是运行时的环境，写法各不相同，只认名字。
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
    """`.env.example` 里的名字：注释掉的也算（惯例是整份都注掉，按需取消）。"""
    return set(
        re.findall(r"^#?\s*(JOY_[A-Z0-9_]+)=", ENV_SAMPLE.read_text(encoding="utf-8"), re.M)
    )

def main() -> int:
    code = (code_vars() | INDIRECT.keys()) - INTERNAL.keys()
    sample = sample_vars()
    doc = {path: set(WORD_RE.findall(path.read_text(encoding="utf-8"))) for path in DOCS}

    if len(code) < MIN_VARS:
        fail(
            f"只从代码里解析出 {len(code)} 个 JOY_*（预期 ≥{MIN_VARS}）"
            " —— 抽取方式大概失效了（换了 env 读取函数或挪了目录），先修这个脚本"
        )

    if missing := sorted(code - sample):
        fail(
            f"{ENV_SAMPLE.name} 里少了 {len(missing)} 个开关：{', '.join(missing)}\n"
            "  抄样例的人开不出它们。要么补上（带一句注释），要么证明它只服务测试、"
            "写进脚本的 INTERNAL（并说明理由）"
        )

    for path, mentions in doc.items():
        if missing := sorted(code - mentions):
            fail(
                f"{path.name} 没提到 {len(missing)} 个开关：{', '.join(missing)}\n"
                "  读者会以为这些开关不存在"
            )

    for name, mentions in [(".env.example", sample), *((p.name, m) for p, m in doc.items())]:
        if stale := sorted(mentions - code - INTERNAL.keys()):
            fail(
                f"{name} 里提到 {len(stale)} 个没人读的开关：{', '.join(stale)}\n"
                "  死开关比没有开关更糟：它看起来能用"
            )

    print(
        f"✓ 配置文档对账：{len(code)} 个开关，"
        f"与 {ENV_SAMPLE.name}、{DOCS[0].name}、{DOCS[1].name} 一致"
        f"（另有 {len(INTERNAL)} 个测试夹具刻意不写）"
    )
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
