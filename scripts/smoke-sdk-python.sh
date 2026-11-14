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

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

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
    echo "✗ that interpreter has no installed joy: $SMOKE_EXPECT_BIN" >&2
    exit 1
  fi
  export SMOKE_EXPECT_BIN
else
  if ! command -v uv >/dev/null 2>&1; then
    echo "✗ cannot find uv — the Python SDK uses it to manage the environment" >&2
    exit 1
  fi
  RUN=(uv run --quiet --project "$REPO/sdk/python" python)
  if [[ ! -x "$BIN" ]]; then
    echo "✗ cannot find $BIN; run: cd joy-rs && cargo build" >&2
    exit 1
  fi
  # JOY_BIN is for find_joy_binary (it reads the environment inside the Python process).
  export JOY_BIN="$BIN"
fi

# Fake SSE endpoint: runs the full "model stream → notifications → full reply" chain without a key.
# It exits after serving one streaming reply.
python3 "$REPO/scripts/fake_sse_server.py" >"$WORK/port" &
FAKE_PID=$!
track_pid "$FAKE_PID"
for _ in $(seq 1 50); do
  [[ -s "$WORK/port" ]] && break
  sleep 0.1
done
FAKE_SSE_PORT="$(head -n1 "$WORK/port" 2>/dev/null || true)"
if [[ -z "$FAKE_SSE_PORT" ]]; then
  echo "✗ the fake SSE endpoint did not come up" >&2
  exit 1
fi

# No other JOY_* vars pass into the subprocess (JOY_BIN set above per mode) — the script assembles
# the environment per case, so the "no key configured" round truly has no key.
export FAKE_SSE_PORT
export SMOKE_WORK="$WORK"

"${RUN[@]}" - <<'PY'
"""Walk a real app-server through it, asserting one thing at a time."""

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
    TurnOperationsParams,
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
    """The no-key path: turn/start should report PROVIDER_ERROR, everything else works."""
    client = await JoyClient.connect(env={**BASE_ENV, "JOY_HOME": f"{WORK}/plain"})
    async with client:
        remembered = await client.request(
            Methods.MEMORY_REMEMBER,
            MemoryRememberParams(subject="Alex", content="Alex likes morning meetings"),
        )
        check(remembered.fact.content == "Alex likes morning meetings", "memory/remember wrote it")

        found = await client.request(Methods.MEMORY_SEARCH, MemorySearchParams(query="morning"))
        check(
            [fact.content for fact in found.facts] == ["Alex likes morning meetings"],
            "the search found the row just written",
        )

        forgotten = await client.request(
            Methods.MEMORY_FORGET, MemoryForgetParams(subject="Alex")
        )
        check(forgotten.removed == 1, "memory/forget deleted it")

        config = await client.request(Methods.CONFIG_READ, ConfigReadParams())
        # Assert on the **inner model**: this only holds if pydantic validation really ran —
        # unpacking a dict by hand would make `config.config` a dict and isinstance a lie.
        check(isinstance(config.config, SettingsView), "config/read came back as a SettingsView")

        # `model/list` isn't implemented yet — exactly what NOT_IMPLEMENTED is for: "in the
        # protocol but not built" and "you called it wrong" are two things, not one error.
        # Two branches so that implementing it someday needs no change to this script.
        try:
            models = await client.request(Methods.MODEL_LIST, ModelListParams())
        except JoyError as error:
            check(
                error.is_not_implemented,
                f"model/list either gives data or says it is not built: {error}",
            )
        else:
            check(isinstance(models.data, list), f"model/list works ({len(models.data)} rows)")

        operations = await client.request(Methods.TURN_OPERATIONS, TurnOperationsParams(sessionId="default"))
        check(operations.operations == [], "ordinary-chat operation inspection works without a model key")

        try:
            await client.request(Methods.TURN_START, TurnStartParams(message="hi"))
            check(False, "with no key, turn/start succeeded — that should not happen")
        except JoyError as error:
            # This is the whole reason JoyError exists: callers branch on code,
            # -32000 should tell the user to configure a key, not look like a bad call.
            check(error.is_provider, f"PROVIDER_ERROR can be branched on by code: {error}")

    # After stdin closes, the server should finish writing queued frames and exit. If close()
    # hangs, that's the hardest bug to spot here (the unit tests' fake transport always closes).
    try:
        await asyncio.wait_for(client.close(), timeout=15)
        closed = True
    except asyncio.TimeoutError:
        closed = False
    check(closed, "after close() the app-server exits on its own (nothing hangs)")
    check(client.closed_reason == "the client closed it", "after close() it is in the closed state")

    try:
        await client.request(Methods.MODEL_LIST, ModelListParams())
        check(False, "a request after close still went out")
    except RuntimeError:
        check(True, "after close a request fails on the spot instead of hanging")

async def streaming() -> None:
    """Model stream → textDelta → the full reply. The far end is fake, so no real key."""
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
            Methods.TURN_START, TurnStartParams(message="hello", stream=True)
        )

    check("textDelta" in kinds, f"notifications reach the subscriber: {kinds}")
    check("".join(deltas) == "hello, world", f"the deltas join up: {deltas}")
    check(replies == ["hello, world"], f"turnCompleted carries the full reply: {replies}")

async def main() -> None:
    # When verifying the published wheel, first pin "the binary comes from the installed package".
    # Without pinning, if find_joy_binary lands on the repo's target (it will when run in-repo),
    # everything below still goes green while proving nothing — which is why this mode exists.
    expected = os.environ.get("SMOKE_EXPECT_BIN")
    if expected:
        resolved = str(find_joy_binary())
        check(resolved == expected, f"the binary comes from the installed package: {resolved}")

    print("[no key]")
    await without_a_key()
    print("[streaming]")
    await streaming()

asyncio.run(main())
PY

echo "✓ Python SDK smoke test passed"
