#!/usr/bin/env bash
# Python SDK 的端到端冒烟测试：真的起一个 `joy app-server`，让客户端去问它。
#
# 单测（`sdk/python/tests/`）把传输层换成了一根内存管子，所以**有一整层它证明
# 不了**：子进程起没起来、"一行一帧"切得对不对、进程没了挂着的 await 收不收敛、
# 关掉 stdin 之后那个进程是不是真退了。这几件事只能拿真进程验。
#
#   ./scripts/smoke-sdk-python.sh
#
# 需要：cargo build 过、uv、python3。

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
FAKE_PID=""
cleanup() {
  [[ -n "$FAKE_PID" ]] && kill "$FAKE_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# 两种跑法：
#
#   默认                       —— 跑仓库里的源码（`uv run --project sdk/python`），
#                                二进制用 `joy-rs/target/debug/joy`。
#   PYTHON=<venv 里的 python>  —— 跑一个**装好的**解释器。验发布出去的轮子用这条：
#                                故意不设 JOY_BIN，让包自己去找，它必须找到包装进来
#                                的那个（下面拿 SMOKE_EXPECT_BIN 钉住这一条）。
#
# 为什么默认那条不能验轮子：它把 JOY_BIN 指到仓库的 target，于是不管包装进来的是
# 什么，用例都会全绿 —— 看上去验了，其实一行都没验到。
if [[ -n "${PYTHON:-}" ]]; then
  RUN=("$PYTHON")
  # PYTHON 既然指向那个 venv，问它自己最准 —— 不用在这里猜 site-packages 的层级。
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
  # JOY_BIN 是给 find_joy_binary 看的（它在 Python 进程里读环境）。
  export JOY_BIN="$BIN"
fi

# 假 SSE 端点：不用真 key，也能把「模型流 → 通知 → 完整应答」整条走完。
# 它服务完一轮流式应答就自己退场。
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

# 其余 JOY_* 一概不传进子进程（JOY_BIN 上面已经按跑法定好了）—— 脚本自己按用例
# 拼环境，好让「没配 key」那一轮真的是没配 key。
export FAKE_SSE_PORT
export SMOKE_WORK="$WORK"

"${RUN[@]}" - <<'PY'
"""拿真的 app-server 走一遍，逐条断言。"""

import asyncio
import os
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
# 把 JOY_* 剥干净：环境里可能残留着真 key，那样「没配 key」这一轮就测不出东西。
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
        # 断言套在**里层的模型**上：这一条只有 pydantic 校验真跑过才成立 ——
        # 拿到一个 dict 再自己拆的话，`config.config` 是个 dict，isinstance 就假了。
        check(isinstance(config.config, SettingsView), "config/read 还原成了 SettingsView")

        # `model/list` 这个阶段还没实现 —— 那正好是 NOT_IMPLEMENTED 的用处：
        # 「协议里有、还没做」跟「你调错了」对用户是两件事，不该混成一个错。
        # 写成两支是为了它哪天实现了不用回来改这个脚本。
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
            # 这一条是 JoyError 存在的全部理由：调用方靠 code 分流，
            # -32000 要提示用户去配 key，而不是当成自己调错了。
            check(error.is_provider, f"PROVIDER_ERROR 能按 code 分流：{error}")

    # 关掉 stdin 之后服务端该把排着的帧写完再退。收不住就是挂住，
    # 而挂住是这里最难发现的坏法（单测里的假传输层永远关得动）。
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
    env = {
        **BASE_ENV,
        "JOY_HOME": f"{WORK}/stream",
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
        # 判别式就是 `type` —— 跟 TS 那边 `n.type === "textDelta"` 是同一个写法。
        # （生成物里那些具名通知类不能用来 isinstance，理由见 protocol.py。）
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
    # 验发布出去的轮子时，先把「二进制来自装进来的那个包」钉死。不钉的话，一旦
    # find_joy_binary 落到仓库的 target 上（在仓库里跑就会），下面照样全绿，
    # 却什么都没证明 —— 而那恰恰是这个模式存在的理由。
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
