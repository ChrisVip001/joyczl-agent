"""The client for the app-server.

It does three things and nothing else:

1. give every request an id, and hand each response back to the `await` that asked for it;
2. fan notifications out to subscribers — **no interpreting, no caching, no replaying**. A turn's
   progress events belong to the caller that is in that turn; a client remembering them would
   only remember a second copy of the state;
3. the moment the process dies, reject every pending request in one go. Otherwise a caller waits
   forever, and "waits forever" is the hardest kind of breakage to trace.

The same thing as `packages/client/src/client.ts` on the TS side.
"""

from __future__ import annotations

import asyncio
import json
import sys
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import Any, Literal, overload

from pydantic import BaseModel, TypeAdapter, ValidationError

from .protocol import (

    ApprovalRespondParams,
    ApprovalRespondResponse,
    Codes,
    ConfigReadParams,
    ConfigReadResponse,
    ConfigWriteParams,
    ConfigWriteResponse,
    ErrorObject,
    MemoryForgetParams,
    MemoryForgetResponse,
    MemoryListEpisodesParams,
    MemoryListEpisodesResponse,
    MemoryListParams,
    MemoryListResponse,
    MemoryValidityParams,
    MemoryValidityResponse,
    MemoryProvenanceParams,
    MemoryProvenanceResponse,
    MemoryRememberParams,
    MemoryRememberResponse,
    MemorySearchParams,
    MemorySearchResponse,
    Methods,
    ModelListParams,
    ModelListResponse,
    ModelProfilesParams,
    ModelProfilesResponse,
    ModelProfileView,
    ModelRemoveParams,
    ModelRoutesResponse,
    ModelRoutesSaveParams,
    RunResponse,
    InboxListView,
    InboxSummarizeParams,
    InboxSummarizeResponse,
    ApprovalLogEntry, ApprovalLogParams, ApprovalLogResponse, ModelRemoveResponse,
    ScheduleListParams, ScheduleListResponse, ScheduleStateRow,
    UsageByModel, UsageSummaryParams, UsageSummaryResponse,
    ModelSaveParams,
    ModelSaveResponse,
    NOTIFICATION_METHOD,
    Notification,
    SessionListParams,
    SessionListResponse,
    SessionMessagesParams,
    SessionMessagesResponse,
    SessionNewParams,
    SessionNewResponse,
    SessionGetResponse,
    SessionUpdateResponse,
    TurnListResponse,
    TurnSteerResponse,
    TurnFollowUpResponse,
    TaskArtifactsResponse,
    TaskRunsResponse,
    TaskActionParams,
    TaskCreateParams,
    TaskStartParams,
    TaskGetParams,
    TaskOperationsResponse,
    TaskReconcileParams,
    TurnOperationsParams,
    TurnReconcileParams,
    TaskWaitParams,
    TaskSignalParams,
    TaskSignalResponse,
    TaskEnvelope,
    TaskListParams,
    TaskListResponse,
    TaskViewResponse,
    TodoListView,
    TurnInterruptParams,
    TurnInterruptResponse,
    TurnStartParams,
    TurnStartResponse,

)
from .transport import StdioTransport, Transport

#: Parse once rather than rebuild per frame — this is the road `textDelta` and its
#: high-frequency kin take.
#:
#: The `TypeAdapter[Any]` annotation is forced: `Notification` is plucked off `RootModel`
#: at runtime, mypy cannot work it out, and dressing that up would be self-deception. At
#: runtime it is exact.
_NOTIFICATION: TypeAdapter[Any] = TypeAdapter(Notification)

#: Method name → response type. `request` uses it to turn the reply back into a model.
#:
#: TS needs no such layer: a plain object out of `JSON.parse` lets `reply.reply` be read
#: directly. Python does not — without restoring it the user holds a dict, `reply.reply`
#: has to be written `reply["reply"]`, and the `-> TurnStartResponse` on that string of
#: overloads below becomes a lie.
#:
#: Keys are enum members rather than strings: `request` folds `method` into an enum on the
#: way in, so storing and looking up take the same road, and `1` versus `"1"` cannot
#: mismatch silently.
_RESPONSES: dict[Methods, type[BaseModel]] = {
    Methods.TURN_START: TurnStartResponse,
    Methods.TURN_INTERRUPT: TurnInterruptResponse,
    Methods.APPROVAL_RESPOND: ApprovalRespondResponse,
    Methods.TODO_VIEW: TodoListView,
    Methods.TASK_CREATE: TaskEnvelope,
    Methods.TASK_START: TaskEnvelope,
    Methods.TASK_OPERATIONS: TaskOperationsResponse,
    Methods.TURN_OPERATIONS: TaskOperationsResponse,
    Methods.TURN_RECONCILE: TaskOperationsResponse,
    Methods.TASK_RECONCILE: TaskOperationsResponse,
    Methods.TASK_WAIT: TaskEnvelope,
    Methods.TASK_SIGNAL: TaskSignalResponse,
    Methods.TASK_LIST: TaskListResponse,
    Methods.TASK_VIEW: TaskViewResponse,
    Methods.TASK_SUSPEND: TaskEnvelope,
    Methods.TASK_RESUME: TaskEnvelope,
    Methods.TASK_CANCEL: TaskEnvelope,
    Methods.SESSION_LIST: SessionListResponse,
    Methods.SESSION_NEW: SessionNewResponse,
    Methods.SESSION_GET: SessionGetResponse,
    Methods.SESSION_UPDATE: SessionUpdateResponse,
    Methods.TURN_LIST: TurnListResponse,
    Methods.TURN_STEER: TurnSteerResponse,
    Methods.TURN_FOLLOW_UP: TurnFollowUpResponse,
    Methods.TASK_GET: TaskViewResponse,
    Methods.TASK_ARTIFACTS: TaskArtifactsResponse,
    Methods.TASK_RUNS: TaskRunsResponse,
    Methods.SESSION_MESSAGES: SessionMessagesResponse,
    Methods.MEMORY_SEARCH: MemorySearchResponse,
    Methods.MEMORY_LIST: MemoryListResponse,
    Methods.MEMORY_LIST_EPISODES: MemoryListEpisodesResponse,
    Methods.MEMORY_VALIDITY: MemoryValidityResponse,
    Methods.MEMORY_PROVENANCE: MemoryProvenanceResponse,
    Methods.MEMORY_REMEMBER: MemoryRememberResponse,
    Methods.MEMORY_FORGET: MemoryForgetResponse,
    Methods.CONFIG_READ: ConfigReadResponse,
    Methods.CONFIG_WRITE: ConfigWriteResponse,
    Methods.MODEL_LIST: ModelListResponse,
    Methods.MODEL_PROFILES: ModelProfilesResponse,
    Methods.MODEL_SAVE: ModelSaveResponse,
    Methods.MODEL_REMOVE: ModelRemoveResponse,
    Methods.MODEL_ROUTES: ModelRoutesResponse,
    Methods.MODEL_ROUTES_SAVE: ModelRoutesResponse,
    Methods.INBOX_LIST: InboxListView,
    Methods.INBOX_SUMMARIZE: InboxSummarizeResponse,
    Methods.USAGE_SUMMARY: UsageSummaryResponse,
    Methods.APPROVAL_LOG: ApprovalLogResponse,
    Methods.SCHEDULE_LIST: ScheduleListResponse,
}


class JoyError(Exception):
    """An error the server sent back, restored as a JSON-RPC error object.

    The `code` travels with it rather than a sentence alone, because callers **really do
    branch on it**: `PROVIDER_ERROR` means "tell the user to configure a key",
    `NOT_IMPLEMENTED` means "not built yet" rather than "you called it wrong" — two
    different things to a user.
    """

    def __init__(self, object_: ErrorObject) -> None:
        super().__init__(object_.message)
        self.code = object_.code
        self.data = object_.data

    @property
    def is_provider(self) -> bool:
        """An invalid key, a rate limit, a model that does not exist… all of these."""
        return self.code == Codes.PROVIDER_ERROR

    @property
    def is_not_implemented(self) -> bool:
        """In the protocol, not built at this stage yet."""
        return self.code == Codes.NOT_IMPLEMENTED

    @property
    def is_invalid_params(self) -> bool:
        """The arguments are shaped wrong — usually this side and the server disagree on"
        versions."""
        return self.code == Codes.INVALID_PARAMS


class _Pending:
    """A request that has gone out and not come back yet."""

    __slots__ = ("future", "method", "response")

    def __init__(
        self,
        future: asyncio.Future[Any],
        method: Methods,
        response: type[BaseModel],
    ) -> None:
        self.future = future
        self.method = method
        self.response = response


class JoyClient:
    """Think of a `JoyClient` as "a remote control for one app-server process"."""

    def __init__(
        self,
        transport: Transport,
        *,
        on_log: Callable[[str], None] | None = None,
    ) -> None:
        self._transport = transport
        # The server's human-readable lines (stderr) and the frames that will not parse.
        # Sent to stderr by default: these are things that should be seen without stopping
        # the flow, and swallowing them would be the wrong call.
        self._on_log = on_log if on_log is not None else _stderr_log

        #: Keys are ids normalised to strings — see `_as_id`.
        self._pending: dict[str, _Pending] = {}
        self._notification_handlers: list[Callable[[Any], None]] = []
        self._next_id = 1
        self._closed_reason: str | None = None

        transport.on_line(self._handle_line)
        transport.on_log(self._on_log)
        transport.on_close(self._handle_close)

    @classmethod
    async def connect(
        cls,
        *,
        command: str | None = None,
        args: Sequence[str] | None = None,
        cwd: str | Path | None = None,
        env: Mapping[str, str] | None = None,
        on_log: Callable[[str], None] | None = None,
    ) -> JoyClient:
        """Start a `joy app-server` and connect to it. On the TS side the caller assembles
        these two steps."""
        transport = StdioTransport(command=command, args=args, cwd=cwd, env=env)
        client = cls(transport, on_log=on_log)
        await transport.start()
        return client

    async def __aenter__(self) -> JoyClient:
        return self

    async def __aexit__(self, *exc_info: object) -> None:
        await self.close()

    # ── Sending requests ───────────────────────────────────────────

    # The string of `overload`s below is the equivalent of TS's `Methods` interface — the
    # table of "which method takes what, and answers what".
    #
    # Hand-written here: **only the pairing**. Every shape comes from the generated artefacts
    # — a method name on the Rust side is a runtime string constant with no type-level link
    # to its `*Params`, so a generator has nothing to derive the pairing from and it has to
    # be written once, here. Where it cannot go wrong: method names are generated enum
    # members (rename in Rust and this raises AttributeError), arguments and responses are
    # the generated types (rename a field and this follows). The only thing left to drift
    # is "paired with the wrong one", and mypy reports that at the call site, because the
    # shapes do not fit.

    @overload
    async def request(
        self, method: Literal[Methods.TURN_OPERATIONS], params: TurnOperationsParams
    ) -> TaskOperationsResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.TURN_RECONCILE], params: TurnReconcileParams
    ) -> TaskOperationsResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.TASK_OPERATIONS], params: TaskGetParams
    ) -> TaskOperationsResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.TASK_RECONCILE], params: TaskReconcileParams
    ) -> TaskOperationsResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.TASK_WAIT], params: TaskWaitParams
    ) -> TaskEnvelope: ...

    @overload
    async def request(
        self, method: Literal[Methods.TASK_SIGNAL], params: TaskSignalParams
    ) -> TaskSignalResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.TASK_START], params: TaskStartParams
    ) -> TaskEnvelope: ...

    @overload
    async def request(
        self, method: Literal[Methods.TURN_START], params: TurnStartParams
    ) -> TurnStartResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.TURN_INTERRUPT], params: TurnInterruptParams
    ) -> TurnInterruptResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.APPROVAL_RESPOND], params: ApprovalRespondParams
    ) -> ApprovalRespondResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.SESSION_LIST], params: SessionListParams
    ) -> SessionListResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.SESSION_NEW], params: SessionNewParams
    ) -> SessionNewResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.SESSION_MESSAGES], params: SessionMessagesParams
    ) -> SessionMessagesResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.MEMORY_SEARCH], params: MemorySearchParams
    ) -> MemorySearchResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.MEMORY_LIST], params: MemoryListParams
    ) -> MemoryListResponse: ...

    @overload
    async def request(
        self,
        method: Literal[Methods.MEMORY_LIST_EPISODES],
        params: MemoryListEpisodesParams,
    ) -> MemoryListEpisodesResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.MEMORY_VALIDITY], params: MemoryValidityParams
    ) -> MemoryValidityResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.MEMORY_PROVENANCE], params: MemoryProvenanceParams
    ) -> MemoryProvenanceResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.MEMORY_REMEMBER], params: MemoryRememberParams
    ) -> MemoryRememberResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.MEMORY_FORGET], params: MemoryForgetParams
    ) -> MemoryForgetResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.CONFIG_READ], params: ConfigReadParams
    ) -> ConfigReadResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.CONFIG_WRITE], params: ConfigWriteParams
    ) -> ConfigWriteResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.MODEL_LIST], params: ModelListParams
    ) -> ModelListResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.MODEL_PROFILES], params: ModelProfilesParams
    ) -> ModelProfilesResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.MODEL_SAVE], params: ModelSaveParams
    ) -> ModelSaveResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.MODEL_REMOVE], params: ModelRemoveParams
    ) -> ModelRemoveResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.USAGE_SUMMARY], params: UsageSummaryParams
    ) -> UsageSummaryResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.APPROVAL_LOG], params: ApprovalLogParams
    ) -> ApprovalLogResponse: ...

    @overload
    async def request(
        self, method: Literal[Methods.SCHEDULE_LIST], params: ScheduleListParams
    ) -> ScheduleListResponse: ...

    async def request(self, method: Methods, params: BaseModel) -> Any:
        """Call a method and wait for its response.

        The argument is required — even "carrying nothing" is written as an empty model. An
        optional second argument would make "forgot to pass it" and "passed nothing" look
        alike, and in this protocol they usually are not.
        """
        if self._closed_reason is not None:
            raise RuntimeError(f"the connection is gone: {self._closed_reason}")

        # Folded into an enum, so storing and looking up `_RESPONSES` take that one road, and
        # a plain string still matches. Unrecognised is said on the spot — sending it and
        # getting a METHOD_NOT_FOUND back is more roundabout, and that error cannot tell
        # "the method name is misspelt" from "the server has no such method".
        try:
            method = Methods(method)
        except ValueError:
            raise ValueError(
                f"this side does not know that method: {method!r} — usually this side and the"
                " server are on different versions"
            ) from None

        response = _RESPONSES.get(method)
        if response is None:
            # Present in the generated artefacts, absent from the pairing table — only
            # possible if Rust added a method and this side did not follow. Say so, rather
            # than leaving the caller to guess at a KeyError.
            raise NotImplementedError(
                f"{method} has no response type paired yet — add it to `_RESPONSES` (and to"
                " the overloads above)"
            )

        request_id = self._next_id
        self._next_id += 1

        future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        self._pending[_as_id(request_id)] = _Pending(future, method, response)
        # Omit absent optional values, but preserve required JSON null data.
        payload = params.model_dump(exclude_none=True, by_alias=True)
        if isinstance(payload, dict):
            for name, field in type(params).model_fields.items():
                if field.is_required() and getattr(params, name) is None:
                    payload[field.alias or name] = None
        self._transport.write(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": str(method),
                    "params": payload,
                }
            )
        )
        return await future

    # ── Subscribing ─────────────────────────────────────────────

    def on_notification(self, handler: Callable[[Any], None]) -> Callable[[], None]:
        """Subscribe to progress events. Returns the unsubscribe function."""
        self._notification_handlers.append(handler)

        def unsubscribe() -> None:
            try:
                self._notification_handlers.remove(handler)
            except ValueError:
                pass

        return unsubscribe

    async def close(self) -> None:
        """A civil close: take no new requests, wait for the queued frames to be written."""
        if self._closed_reason is not None:
            return
        self._closed_reason = "the client closed it"
        await self._transport.close()
        self._reject_all(self._closed_reason)

    @property
    def closed_reason(self) -> str | None:
        """Why the connection ended; None while it is still up."""
        return self._closed_reason

    # ── Receiving frames ────────────────────────────────────────

    def _handle_line(self, line: str) -> None:
        try:
            frame = json.loads(line)
        except json.JSONDecodeError:
            # The server only writes valid JSON; if it really did write something broken then
            # it has a problem of its own — but one bad line should not take the client down
            # with it. Shake it out for a human to see.
            self._on_log(f"this frame will not parse, pasted as it came: {line}")
            return

        if not isinstance(frame, dict):
            self._on_log(f"this frame is not an object: {line}")
            return

        if "method" in frame:
            self._handle_notification(frame)
            return

        if "error" in frame:
            self._handle_error(frame)
            return

        key = _as_id(frame.get("id"))
        pending = self._pending.pop(key, None)
        if pending is None:
            self._on_log(f"nobody is waiting for this response id: {frame.get('id')!r}")
            return
        if pending.future.done():
            return
        try:
            pending.future.set_result(pending.response.model_validate(frame.get("result")))
        except ValidationError as error:
            # A shape mismatch is the classic symptom of "this side and the server are not
            # on the same protocol version", and with the symptom right there, that is the
            # thing to say.
            pending.future.set_exception(
                RuntimeError(
                    f"the response to {pending.method} does not match this side's protocol"
                    f" ({error.error_count()} problems) — usually this side and the server"
                    f" are on different versions"
                )
            )

    def _handle_notification(self, frame: dict[str, Any]) -> None:
        if frame.get("method") != NOTIFICATION_METHOD:
            self._on_log(f"unknown notification method: {frame.get('method')!r}")
            return

        params = frame.get("params")
        if params is None or not isinstance(params, dict):
            self._on_log(f"the notification carries nothing: {json.dumps(frame)}")
            return

        # Here this parts ways with TS. TS **deliberately validates nothing**, on the grounds
        # that "copying the validation into the client is just another contract that will
        # drift" — but that only holds for hand-written validation. This one is generated by
        # pydantic from the same schema the server uses, same origin, cannot drift; and
        # without it a subscriber holds a dict, and `n.delta` has to be written
        # `n["delta"]`.
        #
        # The price is one validation per frame. A genuinely misshapen frame is still the
        # server's bug — the difference is that it is said here, on the spot, rather than
        # blowing up whenever some handler reaches for that field.
        try:
            notification = _NOTIFICATION.validate_python(params)
        except ValidationError as error:
            self._on_log(
                f"the notification's shape does not match ({error.error_count()} problems), "
                f"pasted as it came: "
                f"{json.dumps(params)}"
            )
            return

        for handler in list(self._notification_handlers):
            handler(notification)

    def _handle_error(self, frame: dict[str, Any]) -> None:
        error = _error_object(frame.get("error"))
        if error is None:
            self._on_log(f"the error object will not parse: {json.dumps(frame)}")
            return

        # A request that will not parse leaves the server no id to give (JSON-RPC says null)
        # — no request can take this error, so it can only be noise.
        if frame.get("id") is None:
            self._on_log(error.message)
            return

        key = _as_id(frame.get("id"))
        pending = self._pending.pop(key, None)
        if pending is None:
            self._on_log(f"nobody is waiting for this error's id: {error.message}")
            return
        if not pending.future.done():
            pending.future.set_exception(JoyError(error))

    def _handle_close(self, reason: str) -> None:
        if self._closed_reason is None:
            self._closed_reason = reason
        self._reject_all(reason)

    def _reject_all(self, reason: str) -> None:
        pending = list(self._pending.values())
        self._pending.clear()
        for entry in pending:
            if entry.future.done():
                continue
            # Carry the method name: the hardest thing about an await that hangs forever is
            # not knowing which request it is waiting on.
            entry.future.set_exception(
                RuntimeError(f"{entry.method} never got a response; the connection went down: "
                f"{reason}")
            )


def _error_object(raw: Any) -> ErrorObject | None:
    """Read a frame's `error` as an `ErrorObject`; None if it will not parse.

    Unparseable still gets raised — an error object without even a `code` invalidates every
    bit of the caller's branching on `code`, and shaking it out as it came is better.
    """
    try:
        return ErrorObject.model_validate(raw)
    except ValidationError:
        return None


def _stderr_log(text: str) -> None:
    print(text, file=sys.stderr)


def _as_id(request_id: Any) -> str:
    """Normalise a frame's id into a `_pending` key.

    A JSON-RPC id may be a number or a string (that is how `RequestId` defines it), and both
    have to pair back through the same road — so it is stored as a string, without
    guessing the type.

    Both storing and looking up **must** go through here: a dict does not convert `1` and
    `"1"` for you, and a numeric key with a string lookup simply does not match —
    silently, so that `await` hangs forever.
    """
    return str(request_id)
