"""客户端自己的逻辑：id 配对、通知分发、出错、连接断了。

这里不起真进程 —— 那是 `scripts/smoke-sdk-python.sh` 的事，它拿真的 app-server
跑完整一个回合。这里只钉住「客户端对帧做了什么」，因为那才是容易错的部分。
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import Callable
from typing import Any

import pytest
from pydantic import BaseModel

from joyczl_agent import (
    JoyClient,
    JoyError,
    Methods,
    ModelListParams,
    Transport,
    TurnStartParams,
    protocol,
)
from joyczl_agent.client import _RESPONSES
from joyczl_agent.generated import v2


class FakeTransport(Transport):
    """把 app-server 换成一根内存管子。"""

    def __init__(self) -> None:
        self.written: list[dict[str, Any]] = []
        self.logs: list[str] = []
        self.started = False
        self.closed = False
        self._line: list[Callable[[str], None]] = []
        self._log: list[Callable[[str], None]] = []
        self._close: list[Callable[[str], None]] = []

    def write(self, line: str) -> None:
        self.written.append(json.loads(line))

    def on_line(self, handler: Callable[[str], None]) -> None:
        self._line.append(handler)

    def on_log(self, handler: Callable[[str], None]) -> None:
        self._log.append(handler)

    def on_close(self, handler: Callable[[str], None]) -> None:
        self._close.append(handler)

    async def start(self) -> None:
        self.started = True

    async def close(self) -> None:
        self.closed = True

    def feed(self, frame: dict[str, Any]) -> None:
        for handler in list(self._line):
            handler(json.dumps(frame))

    def feed_raw(self, line: str) -> None:
        for handler in list(self._line):
            handler(line)

    def die(self, reason: str) -> None:
        for handler in list(self._close):
            handler(reason)


async def _connected() -> tuple[FakeTransport, JoyClient]:
    transport = FakeTransport()
    client = JoyClient(transport, on_log=transport.logs.append)
    await transport.start()
    return transport, client


async def _fire(
    client: JoyClient,
    transport: FakeTransport,
    method: str,
    params: BaseModel,
) -> asyncio.Task[Any]:
    """发一个请求，等它写到线上，把任务交回来让测试自己决定喂什么。

    `method` / `params` 故意标宽：这是个转发器，把 `request` 那套重载抹平了 ——
    重载是给用户看的，这里只需要「发出去」。
    """
    task = asyncio.create_task(
        client.request(method, params)  # type: ignore[call-overload]
    )
    await asyncio.sleep(0)
    return task


async def _abandon(task: asyncio.Task[Any]) -> None:
    task.cancel()
    await asyncio.gather(task, return_exceptions=True)


def test_request_pairs_the_response_back_by_id() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        task = await _fire(client, transport, Methods.MODEL_LIST, ModelListParams())

        sent = transport.written[0]
        assert sent["jsonrpc"] == "2.0"
        assert sent["method"] == "model/list"

        transport.feed({"jsonrpc": "2.0", "id": sent["id"], "result": {"data": []}})
        reply = await task
        # 还原成模型，不是 dict —— 不然 `-> ModelListResponse` 那句标注是假的。
        assert reply.data == []

    asyncio.run(scenario())


def test_unset_optional_params_are_left_out() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        task = await _fire(
            client, transport, Methods.TURN_START, TurnStartParams(message="你好")
        )

        # 「没填」该是不出现，而不是出现成 null —— TS 那边 JSON.stringify 丢
        # undefined 是这个动作，Rust 侧 serde 的 Option 也是这么读的。
        assert transport.written[0]["params"] == {"message": "你好"}
        await _abandon(task)

    asyncio.run(scenario())


def test_notification_reaches_the_handler_as_a_model() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        seen: list[Any] = []
        client.on_notification(seen.append)

        transport.feed(
            {
                "jsonrpc": "2.0",
                "method": "turn/notification",
                "params": {"type": "textDelta", "turnId": "t1", "delta": "你"},
            }
        )

        assert len(seen) == 1
        # `type` 是 Literal，能直接跟字符串比 —— 这是把 `--enum-field-as-literal`
        # 设成 `one` 而不是 `all` 换来的。
        assert seen[0].type == "textDelta"
        assert seen[0].delta == "你"
        assert transport.logs == []

    asyncio.run(scenario())


def test_notification_that_does_not_fit_is_reported_not_raised() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        seen: list[Any] = []
        client.on_notification(seen.append)

        # 少了 delta。
        transport.feed(
            {
                "jsonrpc": "2.0",
                "method": "turn/notification",
                "params": {"type": "textDelta", "turnId": "t1"},
            }
        )

        # 不发给订阅者（它拿到的会是个残缺对象），但也不能让客户端崩。
        assert seen == []
        assert len(transport.logs) == 1
        assert "形状对不上" in transport.logs[0]

    asyncio.run(scenario())


def test_a_line_that_is_not_json_is_reported_not_raised() -> None:
    async def scenario() -> None:
        transport, _client = await _connected()
        transport.feed_raw("{这不是 JSON")
        assert len(transport.logs) == 1
        assert "读不出来" in transport.logs[0]

    asyncio.run(scenario())


def test_error_response_becomes_a_joy_error() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        task = await _fire(client, transport, Methods.MODEL_LIST, ModelListParams())
        transport.feed(
            {
                "jsonrpc": "2.0",
                "id": transport.written[0]["id"],
                "error": {"code": -32000, "message": "模型那边没接上"},
            }
        )

        with pytest.raises(JoyError) as caught:
            await task
        assert str(caught.value) == "模型那边没接上"
        # 调用方靠 code 分流：-32000 是「去配 key」，不是「你调错了」。
        assert caught.value.is_provider
        assert not caught.value.is_not_implemented

    asyncio.run(scenario())


def test_server_echoing_the_id_as_a_string_still_pairs() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        task = await _fire(client, transport, Methods.MODEL_LIST, ModelListParams())
        ours = transport.written[0]["id"]

        # `RequestId` 是 `number | string`，两种都得能配回来。
        transport.feed({"jsonrpc": "2.0", "id": str(ours), "result": {"data": []}})
        assert (await task).data == []

    asyncio.run(scenario())


def test_the_process_dying_rejects_everything_still_waiting() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        task = await _fire(client, transport, Methods.MODEL_LIST, ModelListParams())

        transport.die("退出（code=1）")

        with pytest.raises(RuntimeError) as caught:
            await task
        # 方法名要带上 —— 一个永远挂着的 await 最难查的地方，就是不知道它在等哪个请求。
        assert "model/list" in str(caught.value)
        assert client.closed_reason == "退出（code=1）"

    asyncio.run(scenario())


def test_request_after_close_fails_fast() -> None:
    async def scenario() -> None:
        _transport, client = await _connected()
        await client.close()
        with pytest.raises(RuntimeError, match="连接已经没了"):
            await client.request(Methods.MODEL_LIST, ModelListParams())

    asyncio.run(scenario())


def test_a_method_this_side_does_not_know_is_rejected_locally() -> None:
    async def scenario() -> None:
        _transport, client = await _connected()
        # 发出去只会换回一句服务端的 METHOD_NOT_FOUND，而那个错分不清
        #「名字打错了」和「服务端没有这个方法」。
        with pytest.raises(ValueError, match="不认识这个方法"):
            # 传的就是个不该被接受的东西，重载自然一个都不匹配。
            await client.request("memory/teleport", ModelListParams())  # type: ignore[call-overload]

    asyncio.run(scenario())


def test_every_method_has_a_response_pairing() -> None:
    """Rust 加了方法、生成物跟上了、配对表没跟上时，这条会红。

    这是 `_RESPONSES` 唯一的防线：少了它，新方法要等到第一次真被调用时才炸，
    而那时候的症状（`NotImplementedError`）离原因（配对表少一行）已经很远了。
    """
    missing = sorted(m.name for m in set(Methods) - set(_RESPONSES))
    extra = sorted(m.name for m in set(_RESPONSES) - set(Methods))
    assert not missing and not extra, (
        f"`Methods` 和 `_RESPONSES` 对不上：生成物里多出 {missing}，配对表里多出 {extra}"
    )


def test_every_method_type_reaches_the_package() -> None:
    """`protocol.py` 那份清单落后于生成物时，这条会红。

    清单是手写的（Python 的静态类型跟不上算出来的再导出，见 `protocol.py` 开头），
    而落后的症状很隐蔽：用户写 `from joyczl_agent import 某Params` 扑空，
    但包自己一切正常 —— 所以在这里钉住。

    只看 `*Params` / `*Response`：具名通知类（`TextDeltaNotification` 那些）是
    故意不导出的，理由见 `protocol.py`。

    也放过 `JsonRpc*`：那是帧信封，`JsonRpcResponse` 恰好也以 Response 结尾，
    但客户端自己拼帧，从不用这些类型。
    """
    missing = sorted(
        name
        for name in dir(v2)
        if name.endswith(("Params", "Response"))
        and not name.startswith("JsonRpc")
        and name not in protocol.__all__
    )
    assert not missing, f"这些方法类型没从 `protocol` 出去：{missing}"


def test_the_named_notification_classes_are_not_exported() -> None:
    """那 12 个具名通知类是**陷阱**，不能放出去。

    它们跟联合里内联的那一份不是同一个类，`isinstance(n, TextDeltaNotification)`
    会是 False —— 导出等于告诉用户「拿这个去 isinstance」。哪天 Rust 侧把 schema
    改成 `allOf: [$ref]`（那样两边就合一了），这条测试会红，那时再放开它们。
    """
    leaked = sorted(
        name
        for name in dir(v2)
        if name.endswith("Notification")
        and not name.startswith("Server")
        and name in protocol.__all__
    )
    assert not leaked, f"这些具名通知类混进了导出面（isinstance 对它们不成立）：{leaked}"
