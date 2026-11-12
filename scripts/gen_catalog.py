#!/usr/bin/env python3
"""从 models.dev 生成 models-catalog.json 的条目 —— 加厂商不用改代码。

    python3 scripts/gen_catalog.py --list                  # 列出 models.dev 的厂商
    python3 scripts/gen_catalog.py <id>                    # 生成一家厂商的目录行（stdout）
    python3 scripts/gen_catalog.py <id> --model <模型名>    # 指定主模型
    python3 scripts/gen_catalog.py <id> --out <路径>        # 合并进 models-catalog.json（默认只打印）

数据源是 https://models.dev/api.json（MIT，OpenCode 团队维护的模型元数据库），
只有标准库，无第三方依赖。生成的行遵循 `models-catalog.json` 的格式
（见 docs/configuration.md「模型目录」）：已知 joy id 是补丁，新 id 是新厂商。

models.dev 不替你做选择：主模型默认取「支持工具调用里上下文最大的」，这只是
起点 —— 默认模型与小模型是人该拍板的事，输出时会列出候选提醒你确认。
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.request
from pathlib import Path

MODELS_DEV_URL = "https://models.dev/api.json"

# joy 侧已知的映射：joy id → (models.dev id, wire, key_env)。
# wire 是 joy 的两种传输格式之一（anthropic / openai），models.dev 不区分这个。
# key_env 留 None = 新厂商，用 models.dev 的 env 字段（没有就 JOY_<ID>_API_KEY）。
KNOWN: dict[str, tuple[str, str, str | None]] = {
    "anthropic": ("anthropic", "anthropic", "ANTHROPIC_API_KEY"),
    "openai": ("openai", "responses", "OPENAI_API_KEY"),
    "openrouter": ("openrouter", "openai", "OPENROUTER_API_KEY"),
    "gemini": ("google", "openai", "GEMINI_API_KEY"),
    "deepseek": ("deepseek", "openai", "DEEPSEEK_API_KEY"),
    "minimax": ("minimax", "anthropic", "MINIMAX_API_KEY"),
    "kimi": ("moonshotai", "anthropic", "MOONSHOT_API_KEY"),
    "glm": ("zhipuai", "anthropic", "ZHIPU_API_KEY"),
    "xai": ("xai", "openai", "XAI_API_KEY"),
    "opencode_zen": ("opencode", "openai", "OPENCODE_ZEN_API_KEY"),
    "opencode_go": ("opencode-go", "openai", "OPENCODE_GO_API_KEY"),
    "ollama": ("ollama", "openai", ""),  # 本地端点免 key
}
# models.dev 的 id 里，joy 走 anthropic wire 的那四家：
ANTHROPIC_WIRE = {"anthropic", "moonshotai", "zhipuai", "minimax"}


def fetch() -> dict:
    req = urllib.request.Request(
        MODELS_DEV_URL, headers={"User-Agent": "joyczl-gen-catalog/1.0"}
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp)


def list_providers(data: dict) -> None:
    for md_id, info in sorted(data.items()):
        name = info.get("name", "?")
        models = len(info.get("models", {}))
        print(f"{md_id:24} {name}  ({models} 个模型)")


def pick_model(models: dict, wanted: str | None) -> tuple[str, dict]:
    """返回 (模型名, 模型元数据)。--model 给了就精确取；否则挑工具调用里上下文最大的。"""
    if wanted:
        if wanted not in models:
            sys.exit(f"错误：{wanted} 不在该厂商的模型表里。有：{', '.join(sorted(models))}")
        return wanted, models[wanted]

    def sort_key(item):
        meta = item[1]
        limit = (meta.get("limit") or {}).get("context") or 0
        return (bool(meta.get("tool_call")), limit)

    ranked = sorted(
        ((n, m) for n, m in models.items() if isinstance(m, dict)),
        key=sort_key,
        reverse=True,
    )
    if not ranked:
        sys.exit("错误：该厂商在 models.dev 上没有任何模型条目")
    name, _ = ranked[0]
    return name, models[name]


def build_row(data: dict, joy_id: str, model: str | None, small_model: str | None,
              base_url: str | None, key_env: str | None) -> dict:
    known = KNOWN.get(joy_id)
    if known:
        md_id, wire, default_key_env = known
    else:
        # 新厂商：id 直接用 models.dev 的。wire 靠猜（anthropic 系那四家已列全），
        # 其余默认 openai 兼容 —— 这覆盖了绝大多数第三方端点。
        md_id, wire, default_key_env = joy_id, "openai", None
        if md_id in ANTHROPIC_WIRE:
            wire = "anthropic"

    info = data.get(md_id)
    if info is None:
        sys.exit(
            f"错误：models.dev 上没有 '{md_id}'"
            + ("（joy id 经映射得到）" if known else "")
            + f"。先跑 --list 看有哪些。"
        )

    model_name, meta = pick_model(info.get("models", {}), model)
    context = (meta.get("limit") or {}).get("context")
    if not isinstance(context, int) or context <= 0:
        sys.exit(f"错误：模型 {model_name} 没有 context 上限数据，请手工填 context_window")
    vision = "image" in ((meta.get("modalities") or {}).get("input") or [])

    # models.dev 的 env 字段可能是字符串也可能是数组（如 ["ZHIPU_API_KEY"]）——拍平取一个。
    env = info.get("env")
    if isinstance(env, list):
        env = env[0] if env else None
    row: dict = {
        "id": joy_id,
        "wire": wire,
        "key_env": key_env or default_key_env
        or env
        or f"JOY_{joy_id.upper().replace('-', '_')}_API_KEY",
        "model": model_name,
        "context_window": context,
        "vision": vision,
    }
    if small_model:
        row["small_model"] = small_model
    if base_url:
        row["base_url"] = base_url
    elif not known:
        # 内置厂商的 base_url 已在二进制里，补丁不用带；新厂商没有它多半连不上。
        print(
            "提醒：新厂商没给 --base-url，目录行里不会有 —— 端点不对时补上再重启。",
            file=sys.stderr,
        )
    return row


def candidates(models: dict) -> str:
    ranked = sorted(
        models.items(),
        key=lambda kv: (bool(kv[1].get("tool_call")),
                        (kv[1].get("limit") or {}).get("context") or 0),
        reverse=True,
    )[:8]
    return ", ".join(name for name, _ in ranked)


def merge_out(row: dict, out: Path) -> None:
    rows = []
    if out.exists():
        rows = json.loads(out.read_text())
        if not isinstance(rows, list):
            sys.exit(f"错误：{out} 不是 JSON 数组，不肯改写")
    rows[:] = [r for r in rows if r.get("id") != row["id"]]
    rows.append(row)
    out.write_text(json.dumps(rows, ensure_ascii=False, indent=2) + "\n")
    print(f"已写入 {out}（共 {len(rows)} 行）。改完要重启进程才生效。", file=sys.stderr)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("provider", nargs="?", help="joy 侧 provider id（内置 12 家或新名字）")
    ap.add_argument("--list", action="store_true", help="列出 models.dev 上的厂商")
    ap.add_argument("--model", help="主模型（缺省：工具调用里上下文最大的）")
    ap.add_argument("--small-model", help="便宜模型（缺省同主模型）")
    ap.add_argument("--base-url", help="API 端点（新厂商通常需要）")
    ap.add_argument("--key-env", help="key 的环境变量名（缺省照内置表或 models.dev 的 env 字段）")
    ap.add_argument("--out", type=Path, help="合并写入 models-catalog.json（缺省只打印到 stdout）")
    args = ap.parse_args()

    if args.list:
        list_providers(fetch())
        return
    if not args.provider:
        ap.error("给 --list 或一个 provider id")

    data = fetch()
    row = build_row(data, args.provider, args.model, args.small_model,
                    args.base_url, args.key_env)
    md_id = KNOWN.get(args.provider, (args.provider,))[0]
    print(f"# 默认模型是「工具调用里上下文最大的」，请确认；候选：", file=sys.stderr)
    print(f"#   {candidates(data.get(md_id, {}).get('models', {}))}", file=sys.stderr)

    if args.out:
        merge_out(row, args.out)
    else:
        print(json.dumps(row, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
