#!/usr/bin/env python3
"""Generate a models-catalog.json entry from models.dev — adding a vendor needs no code change.

    python3 scripts/gen_catalog.py --list                  # list the vendors models.dev knows
    python3 scripts/gen_catalog.py <id>                    # one vendor's catalog row (stdout)
    python3 scripts/gen_catalog.py <id> --model <name>     # name the main model
    python3 scripts/gen_catalog.py <id> --out <path>       # merge into models-catalog.json

The source is https://models.dev/api.json (MIT, the model metadata database the OpenCode team
maintains), standard library only, no third-party dependency. The row it writes follows the
`models-catalog.json` format (see "模型目录" in docs/configuration.md): a known joy id is a
patch, a new id is a new vendor.

models.dev does not make the choice for you: the main model defaults to "the one with the
largest context among those that support tool calls", which is only a starting point —
the default and small models are a human's call, so the candidates are listed for you to
confirm.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.request
from pathlib import Path

MODELS_DEV_URL = "https://models.dev/api.json"

# The mapping joy already knows: joy id → (models.dev id, wire, key_env).
# `wire` is one of joy's two transports (anthropic / openai); models.dev does not distinguish.
# A key_env of None means a new vendor: take models.dev's env field (or JOY_<ID>_API_KEY).
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
    "ollama": ("ollama", "openai", ""),  # a local endpoint needs no key
}
# Of models.dev's ids, the four whose joy side goes over the anthropic wire:
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
        print(f"{md_id:24} {name}  ({models} models)")


def pick_model(models: dict, wanted: str | None) -> tuple[str, dict]:
    """Return (model name, metadata). An explicit --model wins; otherwise take the largest
    context among the tool-calling models."""
    if wanted:
        if wanted not in models:
            sys.exit(f"error: {wanted} is not in this vendor's model table. Available: "
                     f"{', '.join(sorted(models))}")
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
        sys.exit("error: this vendor has no model entries on models.dev at all")
    name, _ = ranked[0]
    return name, models[name]


def build_row(data: dict, joy_id: str, model: str | None, small_model: str | None,
              base_url: str | None, key_env: str | None) -> dict:
    known = KNOWN.get(joy_id)
    if known:
        md_id, wire, default_key_env = known
    else:
        # A new vendor: the id is models.dev's. The wire is a guess (the four anthropic-wire
        # vendors are all listed), everything else defaults to openai-compatible — which covers
        # the overwhelming majority of third-party endpoints.
        md_id, wire, default_key_env = joy_id, "openai", None
        if md_id in ANTHROPIC_WIRE:
            wire = "anthropic"

    info = data.get(md_id)
    if info is None:
        sys.exit(
            f"error: models.dev has no '{md_id}'"
            + (" (that is the joy id after mapping)" if known else "")
            + ". Run --list first to see what is there."
        )

    model_name, meta = pick_model(info.get("models", {}), model)
    context = (meta.get("limit") or {}).get("context")
    if not isinstance(context, int) or context <= 0:
        sys.exit(f"error: model {model_name} has no context limit on models.dev; "
                 "fill in context_window by hand")
    vision = "image" in ((meta.get("modalities") or {}).get("input") or [])

    # models.dev's env field may be a string or an array (e.g. ["ZHIPU_API_KEY"]) — flatten it.
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
        # A built-in vendor's base_url is already in the binary, so the patch need not carry
        # it; a new vendor usually cannot connect without one.
        print(
            "note: this new vendor got no --base-url, so the row will not carry one — add it "
            "and restart if the endpoint is wrong.",
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
            sys.exit(f"error: {out} is not a JSON array; refusing to rewrite it")
    rows[:] = [r for r in rows if r.get("id") != row["id"]]
    rows.append(row)
    out.write_text(json.dumps(rows, ensure_ascii=False, indent=2) + "\n")
    print(f"wrote {out} ({len(rows)} rows). A running process has to restart to pick it up.",
          file=sys.stderr)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("provider", nargs="?", help="the joy-side provider id (one of the 12 built in, or a new name)")
    ap.add_argument("--list", action="store_true", help="list the vendors on models.dev")
    ap.add_argument("--model", help="the main model (default: the largest context among tool-calling ones)")
    ap.add_argument("--small-model", help="the cheap model (defaults to the main one)")
    ap.add_argument("--base-url", help="the API endpoint (usually needed for a new vendor)")
    ap.add_argument("--key-env", help="the environment variable holding the key (default: the built-in table, or models.dev's env)")
    ap.add_argument("--out", type=Path, help="merge into models-catalog.json (default: print to stdout)")
    args = ap.parse_args()

    if args.list:
        list_providers(fetch())
        return
    if not args.provider:
        ap.error("give --list or a provider id")

    data = fetch()
    row = build_row(data, args.provider, args.model, args.small_model,
                    args.base_url, args.key_env)
    md_id = KNOWN.get(args.provider, (args.provider,))[0]
    print("# The default model is «the largest context among tool-calling ones»; please confirm. "
          "Candidates:", file=sys.stderr)
    print(f"#   {candidates(data.get(md_id, {}).get('models', {}))}", file=sys.stderr)

    if args.out:
        merge_out(row, args.out)
    else:
        print(json.dumps(row, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
