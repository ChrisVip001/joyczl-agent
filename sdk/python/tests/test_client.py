"""The client's own logic: id pairing, notification fan-out, errors, a dropped connection.

No real process here — that is `scripts/smoke-sdk-python.sh`'s job, and it runs a whole
turn against a real app-server. This pins down what the client does to a frame, because
that is the part that goes wrong.
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
    """Swap the app-server for an in-memory pipe."""

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
    """Send a request, wait until it is on the wire, and hand the task back so the test can
    decide what to feed it.

    `method` / `params` are deliberately wide: this is a pass-through that flattens the
    overloads on `request` — those are for users; all this needs is "it goes out".
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
        # Restored as a model, not a dict — otherwise the `-> ModelListResponse` annotation
        # would be a lie.
        assert reply.data == []

    asyncio.run(scenario())


def test_unset_optional_params_are_left_out() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        task = await _fire(
            client, transport, Methods.TURN_START, TurnStartParams(message="hello")
        )

        # "Not filled in" should be absent, not present as null — dropping `undefined` is what
        # `JSON.stringify` does on the TS side, and serde's `Option` reads it the same way.
        assert transport.written[0]["params"] == {"message": "hello"}
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
                "params": {"type": "textDelta", "turnId": "t1", "delta": "he"},
            }
        )

        assert len(seen) == 1
        # `type` is a Literal, so it can be compared to a string directly — that is what
        # setting `--enum-field-as-literal` to `one` rather than `all` bought.
        assert seen[0].type == "textDelta"
        assert seen[0].delta == "he"
        assert transport.logs == []

    asyncio.run(scenario())


def test_notification_that_does_not_fit_is_reported_not_raised() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        seen: list[Any] = []
        client.on_notification(seen.append)

        # The delta is missing.
        transport.feed(
            {
                "jsonrpc": "2.0",
                "method": "turn/notification",
                "params": {"type": "textDelta", "turnId": "t1"},
            }
        )

        # Not handed to subscribers (they would get a broken object), but it must not take the
        # client down either.
        assert seen == []
        assert len(transport.logs) == 1
        assert "shape does not match" in transport.logs[0]

    asyncio.run(scenario())


def test_a_line_that_is_not_json_is_reported_not_raised() -> None:
    async def scenario() -> None:
        transport, _client = await _connected()
        transport.feed_raw("{ this is not JSON")
        assert len(transport.logs) == 1
        assert "will not parse" in transport.logs[0]

    asyncio.run(scenario())


def test_error_response_becomes_a_joy_error() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        task = await _fire(client, transport, Methods.MODEL_LIST, ModelListParams())
        transport.feed(
            {
                "jsonrpc": "2.0",
                "id": transport.written[0]["id"],
                "error": {"code": -32000, "message": "the model never connected"},
            }
        )

        with pytest.raises(JoyError) as caught:
            await task
        assert str(caught.value) == "the model never connected"
        # The caller branches on code: -32000 is "go configure a key", not "you called it wrong".
        assert caught.value.is_provider
        assert not caught.value.is_not_implemented

    asyncio.run(scenario())


def test_server_echoing_the_id_as_a_string_still_pairs() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        task = await _fire(client, transport, Methods.MODEL_LIST, ModelListParams())
        ours = transport.written[0]["id"]

        # `RequestId` is `number | string`, and both have to pair back.
        transport.feed({"jsonrpc": "2.0", "id": str(ours), "result": {"data": []}})
        assert (await task).data == []

    asyncio.run(scenario())


def test_the_process_dying_rejects_everything_still_waiting() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        task = await _fire(client, transport, Methods.MODEL_LIST, ModelListParams())

        transport.die("exit (code=1)")

        with pytest.raises(RuntimeError) as caught:
            await task
        # The method name has to travel — the hardest thing about an await that hangs forever
        # is not knowing which request it is waiting on.
        assert "model/list" in str(caught.value)
        assert client.closed_reason == "exit (code=1)"

    asyncio.run(scenario())


def test_request_after_close_fails_fast() -> None:
    async def scenario() -> None:
        _transport, client = await _connected()
        await client.close()
        with pytest.raises(RuntimeError, match="the connection is gone"):
            await client.request(Methods.MODEL_LIST, ModelListParams())

    asyncio.run(scenario())


def test_a_method_this_side_does_not_know_is_rejected_locally() -> None:
    async def scenario() -> None:
        _transport, client = await _connected()
        # Sending it would only get a METHOD_NOT_FOUND back from the server, and that error
        # cannot tell "the name is misspelt" from "the server has no such method".
        with pytest.raises(ValueError, match="does not know that method"):
            # What is passed is something that must not be accepted, so no overload matches.
            await client.request("memory/teleport", ModelListParams())  # type: ignore[call-overload]

    asyncio.run(scenario())


def test_every_method_has_a_response_pairing() -> None:
    """Goes red when Rust adds a method, the artefacts follow, and the pairing table does not.

    This is `_RESPONSES`' only guard: without it a new method only blows up the first time
    it is really called, and by then the symptom (`NotImplementedError`) is a long way from
    the cause (one missing line in the pairing table).
    """
    missing = sorted(m.name for m in set(Methods) - set(_RESPONSES))
    extra = sorted(m.name for m in set(_RESPONSES) - set(Methods))
    assert not missing and not extra, (
        f"`Methods` and `_RESPONSES` disagree: the artefacts have {missing} extra, the pairing "
        f"table has {extra} extra"
    )


def test_every_method_type_reaches_the_package() -> None:
    """Goes red when `protocol.py`'s list falls behind the generated artefacts.

    The list is hand-written (Python's static types cannot follow a computed re-export, see the
    top of `protocol.py`), and falling behind is a quiet symptom: a user writing
    `from joyczl_agent import SomeParams` comes up empty while the package itself is fine —
    so it is pinned here.

    Only `*Params` / `*Response`: the named notification classes (`TextDeltaNotification` and
    kin) are deliberately not exported, see `protocol.py`.

    `JsonRpc*` is let through too: that is the frame envelope, and `JsonRpcResponse` happens to
    end in Response as well, but the client builds its own frames and never uses these.
    """
    missing = sorted(
        name
        for name in dir(v2)
        if name.endswith(("Params", "Response"))
        and not name.startswith("JsonRpc")
        and name not in protocol.__all__
    )
    assert not missing, f"these method types never left `protocol`: {missing}"


def test_the_named_notification_classes_are_not_exported() -> None:
    """Those 12 named notification classes are a **trap** and must not be exported.

    They are not the same class as the copy inlined in the union, so
    `isinstance(n, TextDeltaNotification)` would be False — exporting them tells the user
    "go on, isinstance with this". If Rust ever changes the schema to `allOf: [$ref]` (which
    would merge the two), this test goes red, and then they can be opened up.
    """
    leaked = sorted(
        name
        for name in dir(v2)
        if name.endswith("Notification")
        and not name.startswith("Server")
        and name in protocol.__all__
    )
    assert not leaked, (
        f"these named notification classes leaked into the export surface "
        f"(isinstance does not hold for them): {leaked}"
    )


def test_required_null_input_is_preserved_on_the_wire() -> None:
    async def scenario() -> None:
        transport, client = await _connected()
        params = protocol.TaskSignalParams(
            conditionId="task:input", source="manual", eventKey="task", eventId="event", data=None
        )
        task = await _fire(client, transport, Methods.TASK_SIGNAL, params)
        try:
            assert "data" in transport.written[-1]["params"]
            assert transport.written[-1]["params"]["data"] is None
        finally:
            await _abandon(task)
            await client.close()

    asyncio.run(scenario())
