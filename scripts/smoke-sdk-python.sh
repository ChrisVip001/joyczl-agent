#!/usr/bin/env bash
# End-to-end smoke test for the Python SDK: really start a `joy app-server`, let the client ask.
#
# The unit tests (sdk/python/tests/) swap the transport for an in-memory pipe, so a whole layer is
# unproven: whether the subprocess starts, whether "one frame per line" splits right, whether
# pending awaits settle when the process dies, whether the process really exits after stdin closes.
#
#   ./scripts/smoke-sdk-python.sh
#
# Requires: a cargo build, uv, python3.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
FAKE_PID=""
cleanup() {
  [[ -n "$FAKE_PID" ]] && kill "$FAKE_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# Two ways to run:
#
#   default                  — run the repo source (`uv run --project sdk/python`); the binary is
#                              `joy-rs/target/debug/joy`.
#   PYTHON=<venv python>     — run an **installed** interpreter. This is the wheel-verification
#                              path: JOY_BIN is deliberately unset so the package finds its own
#                              binary (pinned below via SMOKE_EXPECT_BIN).
#
# Why the default path can't verify the wheel: it points JOY_BIN at the repo's target, so cases
# go all green no matter what got installed — it looks verified but verifies nothing.
if [[ -n "${PYTHON:-}" ]]; then
  RUN=("$PYTHON")
  # PYTHON points at that venv, so asking it is most accurate — no guessing site-packages layout.
  SMOKE_EXPECT_BIN="$("$PYTHON" -c 'import joyczl_agent_bin; print(joyczl_agent_bin.binary())')"
  if [[ ! -f "$SMOKE_EXPECT_BIN" ]]; then
    echo "✗ 那个解释器里没有装好的 joy：$SMOKE_EXPECT_BIN" >&2
    exit 1
  fi
  export SMOKE_EXPECT_BIN
else
  if ! command -v uv >/dev/null 2>&1; then
    echo "✗ 找不到 uv —— Python SDK 用它管环境" >&2
    exit 1
  fi
  RUN=(uv run --quiet --project "$REPO/sdk/python" python)
  BIN="${JOY_BIN:-$REPO/joy-rs/target/debug/joy}"
  if [[ ! -x "$BIN" ]]; then
    echo "✗ 找不到 $BIN，先跑：cd joy-rs && cargo build" >&2
    exit 1
  fi
  # JOY_BIN is for find_joy_binary (it reads the environment inside the Python process).
  export JOY_BIN="$BIN"
fi

# Fake SSE endpoint: runs the full "model stream → notifications → full reply" chain without a key.
# It exits after serving one streaming reply.
python3 "$REPO/scripts/fake_sse_server.py" >"$WORK/port" &
FAKE_PID=$!
for _ in $(seq 1 50); do
  [[ -s "$WORK/port" ]] && break
  sleep 0.1
done
FAKE_SSE_PORT="$(head -n1 "$WORK/port" 2>/dev/null || true)"
if [[ -z "$FAKE_SSE_PORT" ]]; then
  echo "✗ 假 SSE 端点没起来" >&2
  exit 1
fi

# No other JOY_* vars pass into the subprocess (JOY_BIN set above per mode) — the script assembles
# the environment per case, so the "no key configured" round truly has no key.
export FAKE_SSE_PORT
export SMOKE_WORK="$WORK"

"${RUN[@]}" - <<'PY'
"""拿真的 app-server 走一遍，逐条断言。"""

import asyncio
import json
import os
import pathlib
import sys

from joyczl_agent import (
    ConfigReadParams,
    JoyClient,
    JoyError,
    MemoryForgetParams,
    MemoryRememberParams,
    MemorySearchParams,
    Methods,
    ModelListParams,
    SettingsView,
    TurnStartParams,
    find_joy_binary,
)

WORK = os.environ["SMOKE_WORK"]
SSE_PORT = os.environ["FAKE_SSE_PORT"]
# Strip JOY_*: real keys may linger in the environment, and then the "no key" round proves nothing.
BASE_ENV = {k: v for k, v in os.environ.items() if not k.startswith("JOY_")}


def check(ok: bool, what: str) -> None:
    print(("  ✓ " if ok else "  ✗ ") + what)
    if not ok:
        sys.exit(1)


async def without_a_key() -> None:
    """没配 key 的那条路：turn/start 该报 PROVIDER_ERROR，别的照常工作。"""
    client = await JoyClient.connect(env={**BASE_ENV, "JOY_HOME": f"{WORK}/plain"})
    async with client:
        remembered = await client.request(
            Methods.MEMORY_REMEMBER,
            MemoryRememberParams(subject="阿明", content="阿明喜欢早上的会议"),
        )
        check(remembered.fact.content == "阿明喜欢早上的会议", "memory/remember 写进去了")

        found = await client.request(Methods.MEMORY_SEARCH, MemorySearchParams(query="早上"))
        check(
            [fact.content for fact in found.facts] == ["阿明喜欢早上的会议"],
            "中文检索命中了刚写的那条",
        )

        forgotten = await client.request(
            Methods.MEMORY_FORGET, MemoryForgetParams(subject="阿明")
        )
        check(forgotten.removed == 1, "memory/forget 删掉了")

        config = await client.request(Methods.CONFIG_READ, ConfigReadParams())
        # Assert on the **inner model**: this only holds if pydantic validation really ran —
        # unpacking a dict by hand would make `config.config` a dict and isinstance a lie.
        check(isinstance(config.config, SettingsView), "config/read 还原成了 SettingsView")

        # `model/list` isn't implemented yet — exactly what NOT_IMPLEMENTED is for: "in the
        # protocol but not built" and "you called it wrong" are two things, not one error.
        # Two branches so that implementing it someday needs no change to this script.
        try:
            models = await client.request(Methods.MODEL_LIST, ModelListParams())
        except JoyError as error:
            check(
                error.is_not_implemented,
                f"model/list 要么给数据、要么明说没做：{error}",
            )
        else:
            check(isinstance(models.data, list), f"model/list 走通了（{len(models.data)} 个）")

        try:
            await client.request(Methods.TURN_START, TurnStartParams(message="hi"))
            check(False, "没配 key 时 turn/start 居然成功了")
        except JoyError as error:
            # This is the whole reason JoyError exists: callers branch on code,
            # -32000 should tell the user to configure a key, not look like a bad call.
            check(error.is_provider, f"PROVIDER_ERROR 能按 code 分流：{error}")

    # After stdin closes, the server should finish writing queued frames and exit. If close()
    # hangs, that's the hardest bug to spot here (the unit tests' fake transport always closes).
    try:
        await asyncio.wait_for(client.close(), timeout=15)
        closed = True
    except asyncio.TimeoutError:
        closed = False
    check(closed, "close() 之后 app-server 自己退了（没挂住）")
    check(client.closed_reason == "客户端主动关闭", "close() 之后进到关闭态")

    try:
        await client.request(Methods.MODEL_LIST, ModelListParams())
        check(False, "关掉之后的请求居然还发得出去")
    except RuntimeError:
        check(True, "关掉之后再发请求当场失败，不会挂住")


async def streaming() -> None:
    """模型流 → textDelta → 完整应答。对端是假的，所以不要真 key。"""
    home = pathlib.Path(WORK) / "stream"
    # Models and keys no longer travel via env vars (see 0.7.0): seed a models.json whose
    # key_source reads JOY_API_KEY from the environment and whose base_url is the fake
    # SSE endpoint. JOY_API_KEY below stays — it is what the env: source reads.
    home.mkdir(parents=True, exist_ok=True)
    (home / "models.json").write_text(
        json.dumps(
            {
                "active": "smoke",
                "profiles": [
                    {
                        "id": "smoke",
                        "provider": "anthropic",
                        "base_url": f"http://127.0.0.1:{SSE_PORT}",
                        "key_source": "env:JOY_API_KEY",
                    }
                ],
            }
        )
    )
    env = {
        **BASE_ENV,
        "JOY_HOME": str(home),
        "JOY_PROVIDER": "anthropic",
        "JOY_API_KEY": "test",
        "JOY_BASE_URL": f"http://127.0.0.1:{SSE_PORT}",
    }
    client = await JoyClient.connect(env=env)

    kinds: list[str] = []
    deltas: list[str] = []
    replies: list[str] = []

    def on_notification(notification) -> None:
        kinds.append(notification.type)
        # The discriminant is `type` — same shape as `n.type === "textDelta"` on the TS side.
        # (The generated named notification classes can't be used with isinstance; see protocol.py.)
        if notification.type == "textDelta":
            deltas.append(notification.delta)
        elif notification.type == "turnCompleted":
            replies.append(notification.reply)

    client.on_notification(on_notification)

    async with client:
        await client.request(
            Methods.TURN_START, TurnStartParams(message="你好", stream=True)
        )

    check("textDelta" in kinds, f"通知到得了订阅者：{kinds}")
    check("".join(deltas) == "你好，世界", f"增量拼得起来：{deltas}")
    check(replies == ["你好，世界"], f"turnCompleted 里是完整答复：{replies}")


async def main() -> None:
    # When verifying the published wheel, first pin "the binary comes from the installed package".
    # Without pinning, if find_joy_binary lands on the repo's target (it will when run in-repo),
    # everything below still goes green while proving nothing — which is why this mode exists.
    expected = os.environ.get("SMOKE_EXPECT_BIN")
    if expected:
        resolved = str(find_joy_binary())
        check(resolved == expected, f"二进制来自装进来的包：{resolved}")

    print("[没配 key]")
    await without_a_key()
    print("[流式]")
    await streaming()


asyncio.run(main())
PY

echo "✓ Python SDK 冒烟测试通过"
