"""app-server 的客户端。

只做三件事，多一件都不做：

1. 给请求配 id、按 id 把应答还给发出请求的那个 `await`；
2. 把通知分发给订阅者 —— **不解释、不缓存、不重放**。一轮对话的过程事件属于当下
   那个调用方，客户端替它记着只会记出另一份状态；
3. 进程一没，把所有挂着的请求一口气拒掉。不然调用方会永远等下去，而「永远等下去」
   是最难查的那种坏法。

跟 TS 侧的 `packages/client/src/client.ts` 是同一个东西。
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
    MemoryRememberParams,
    MemoryRememberResponse,
    MemorySearchParams,
    MemorySearchResponse,
    Methods,
    ModelListParams,
    ModelListResponse,
    NOTIFICATION_METHOD,
    Notification,
    SessionListParams,
    SessionListResponse,
    SessionMessagesParams,
    SessionMessagesResponse,
    SessionNewParams,
    SessionNewResponse,
    TurnInterruptParams,
    TurnInterruptResponse,
    TurnStartParams,
    TurnStartResponse,

)
from .transport import StdioTransport, Transport

#: 解析一次，别每帧都重建 —— 这是 `textDelta` 那一类高频帧要走的路。
#:
#: 标注写成 `TypeAdapter[Any]` 是被迫的：`Notification` 是运行时从 `RootModel`
#: 上摘的，mypy 求不出它，这里写得出花来也是自欺。运行时是准的。
_NOTIFICATION: TypeAdapter[Any] = TypeAdapter(Notification)

#: 方法名 → 应答类型。`request` 用它把回包还原成模型。
#:
#: TS 那边不需要这一层：`JSON.parse` 出来的普通对象，`reply.reply` 直接就能读。
#: Python 不行 —— 不还原的话用户拿到的是 dict，`reply.reply` 得写成 `reply["reply"]`，
#: 而下面那一串 `-> TurnStartResponse` 也就成了句假话。
#:
#: key 用枚举成员而不是字符串：`request` 一进来就把 `method` 收敛成枚举，
#: 存和取走的是同一条路，不会出现 `1` 和 `"1"` 那种配不上还静悄悄的情况。
_RESPONSES: dict[Methods, type[BaseModel]] = {
    Methods.TURN_START: TurnStartResponse,
    Methods.TURN_INTERRUPT: TurnInterruptResponse,
    Methods.APPROVAL_RESPOND: ApprovalRespondResponse,
    Methods.SESSION_LIST: SessionListResponse,
    Methods.SESSION_NEW: SessionNewResponse,
    Methods.SESSION_MESSAGES: SessionMessagesResponse,
    Methods.MEMORY_SEARCH: MemorySearchResponse,
    Methods.MEMORY_LIST: MemoryListResponse,
    Methods.MEMORY_LIST_EPISODES: MemoryListEpisodesResponse,
    Methods.MEMORY_REMEMBER: MemoryRememberResponse,
    Methods.MEMORY_FORGET: MemoryForgetResponse,
    Methods.CONFIG_READ: ConfigReadResponse,
    Methods.CONFIG_WRITE: ConfigWriteResponse,
    Methods.MODEL_LIST: ModelListResponse,
}


class JoyError(Exception):
    """服务端回的错，按 JSON-RPC 的错误对象还原。

    带上 `code` 而不是只留一句话，是因为调用方**真的会按它分流**：
    `PROVIDER_ERROR` 要提示用户去配 key，`NOT_IMPLEMENTED` 是「这功能还没做」
    而不是「你调错了」，两者对用户是两件事。
    """

    def __init__(self, object_: ErrorObject) -> None:
        super().__init__(object_.message)
        self.code = object_.code
        self.data = object_.data

    @property
    def is_provider(self) -> bool:
        """key 无效、限流、模型不存在…… 都是这一类。"""
        return self.code == Codes.PROVIDER_ERROR

    @property
    def is_not_implemented(self) -> bool:
        """协议里有、当前阶段还没做。"""
        return self.code == Codes.NOT_IMPLEMENTED

    @property
    def is_invalid_params(self) -> bool:
        """参数形状不对 —— 多半是本端跟服务端版本对不上。"""
        return self.code == Codes.INVALID_PARAMS


class _Pending:
    """一个发出去了、还没回来的请求。"""

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
    """把一个 `JoyClient` 想成「一个 app-server 进程的遥控器」。"""

    def __init__(
        self,
        transport: Transport,
        *,
        on_log: Callable[[str], None] | None = None,
    ) -> None:
        self._transport = transport
        # 服务端的人话（stderr）与解析不了的帧。默认打到 stderr：
        # 这些是「该被看见但不该中断流程」的东西，吞掉才是错的。
        self._on_log = on_log if on_log is not None else _stderr_log

        #: key 是 id 归一化后的字符串 —— 见 `_as_id`。
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
        """起一个 `joy app-server` 并接上。TS 那边要调用方自己拼这两步。"""
        transport = StdioTransport(command=command, args=args, cwd=cwd, env=env)
        client = cls(transport, on_log=on_log)
        await transport.start()
        return client

    async def __aenter__(self) -> JoyClient:
        return self

    async def __aexit__(self, *exc_info: object) -> None:
        await self.close()

    # ── 出请求 ───────────────────────────────────────────────────────────

    # 下面这一串 `overload` 就是 TS 那个 `Methods` 接口的等价物 ——
    # 「哪个方法收什么、回什么」的对照表。
    #
    # 手写的**只有配对关系**，形状全部来自生成物 —— Rust 侧的方法名是个运行时
    # 字符串常量，跟 `*Params` 类型之间没有任何类型层面的联系，生成器无从推导，
    # 只能在这边写一次。写不错的地方：方法名用生成的枚举成员（Rust 改名这里就
    # 报 AttributeError），参数/应答直接引用生成的类型（字段改名这里跟着变）。
    # 剩下唯一能漂的就是「配错对」，而配错了 mypy 会在调用处报出来，因为形状不合。

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

    async def request(self, method: Methods, params: BaseModel) -> Any:
        """调一个方法，等它的应答。

        参数是必填的 —— 连「什么都不带」也要写成一个空模型。可选的第二参数会让
        「忘了传」和「就传空」看起来一样，而这个协议里它们往往不一样。
        """
        if self._closed_reason is not None:
            raise RuntimeError(f"连接已经没了：{self._closed_reason}")

        # 收敛成枚举：`_RESPONSES` 的存和取就都走这一条路了，传字符串也对得上。
        # 认不出来就当场说 —— 发出去换一句服务端的 METHOD_NOT_FOUND 更绕，
        # 而且那个错分不清「方法名打错了」和「服务端没有这个方法」。
        try:
            method = Methods(method)
        except ValueError:
            raise ValueError(
                f"本端不认识这个方法：{method!r} —— 多半是本端和服务端版本不一致"
            ) from None

        response = _RESPONSES.get(method)
        if response is None:
            # 生成物里有了、配对表里没有 —— 只可能是 Rust 加了方法而这里没跟上。
            # 说清楚，别让调用方对着一句 KeyError 猜。
            raise NotImplementedError(
                f"{method} 还没有配对应答类型 —— 补进 `_RESPONSES`（和上面那串 overload）"
            )

        request_id = self._next_id
        self._next_id += 1

        future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        self._pending[_as_id(request_id)] = _Pending(future, method, response)
        # `exclude_none` 是把 TS 那边 `JSON.stringify` 丢掉 `undefined` 的动作
        # 跟着做一遍：可选字段「没填」时该是不出现，而不是出现成 `null`。
        self._transport.write(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": str(method),
                    "params": params.model_dump(exclude_none=True, by_alias=True),
                }
            )
        )
        return await future

    # ── 订阅 ─────────────────────────────────────────────────────────────

    def on_notification(self, handler: Callable[[Any], None]) -> Callable[[], None]:
        """订阅过程事件。返回退订函数。"""
        self._notification_handlers.append(handler)

        def unsubscribe() -> None:
            try:
                self._notification_handlers.remove(handler)
            except ValueError:
                pass

        return unsubscribe

    async def close(self) -> None:
        """文明关闭：不再收新请求，等排着的帧写完。"""
        if self._closed_reason is not None:
            return
        self._closed_reason = "客户端主动关闭"
        await self._transport.close()
        self._reject_all(self._closed_reason)

    @property
    def closed_reason(self) -> str | None:
        """连接没了的原因；还在就是 None。"""
        return self._closed_reason

    # ── 收帧 ─────────────────────────────────────────────────────────────

    def _handle_line(self, line: str) -> None:
        try:
            frame = json.loads(line)
        except json.JSONDecodeError:
            # 服务端只会写合法 JSON；真写坏了说明它自己也出了事，
            # 但这一句坏掉不该让客户端跟着崩 —— 抖出去给人看。
            self._on_log(f"这一帧读不出来，原样贴出来：{line}")
            return

        if not isinstance(frame, dict):
            self._on_log(f"这一帧不是个对象：{line}")
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
            self._on_log(f"应答的 id 没人在等：{frame.get('id')!r}")
            return
        if pending.future.done():
            return
        try:
            pending.future.set_result(pending.response.model_validate(frame.get("result")))
        except ValidationError as error:
            # 形状对不上是「本端和服务端不是同一个协议版本」的典型症状，
            # 而症状摆在眼前时最该说的是这句话。
            pending.future.set_exception(
                RuntimeError(
                    f"{pending.method} 的应答对不上本端的协议"
                    f"（{error.error_count()} 处）—— 多半是本端和服务端版本不一致"
                )
            )

    def _handle_notification(self, frame: dict[str, Any]) -> None:
        if frame.get("method") != NOTIFICATION_METHOD:
            self._on_log(f"不认识的通知方法：{frame.get('method')!r}")
            return

        params = frame.get("params")
        if params is None or not isinstance(params, dict):
            self._on_log(f"通知没有内容：{json.dumps(frame)}")
            return

        # 这里跟 TS 分了个岔。TS 那边**刻意不校验**，理由是「在客户端再抄一遍
        # 校验逻辑就是又一份会漂的合同」—— 但那话只在手写校验时成立。这里的
        # 校验是 pydantic 从同一份 schema 生成的，跟服务端同源，漂不了；而不用它
        # 的话，订阅者拿到的就是一个 dict，`n.delta` 得写成 `n["delta"]`。
        #
        # 代价是每帧一次校验。真出形状不对的帧，那还是服务端的 bug —— 差别只在
        # 这里当场说清楚，而不是等到某个 handler 用到那个字段时才炸。
        try:
            notification = _NOTIFICATION.validate_python(params)
        except ValidationError as error:
            self._on_log(
                f"通知的形状对不上（{error.error_count()} 处），原样贴出来："
                f"{json.dumps(params)}"
            )
            return

        for handler in list(self._notification_handlers):
            handler(notification)

    def _handle_error(self, frame: dict[str, Any]) -> None:
        error = _error_object(frame.get("error"))
        if error is None:
            self._on_log(f"错误对象读不出来：{json.dumps(frame)}")
            return

        # 解析不了请求时服务端给不了 id（JSON-RPC 规定为 null）——没有请求
        # 能拿这个错，只能当噪音。
        if frame.get("id") is None:
            self._on_log(error.message)
            return

        key = _as_id(frame.get("id"))
        pending = self._pending.pop(key, None)
        if pending is None:
            self._on_log(f"错误的 id 没人在等：{error.message}")
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
            # 把方法名带上：一个永远挂着的 await 最难查的地方就是不知道
            # 它在等哪个请求。
            entry.future.set_exception(
                RuntimeError(f"{entry.method} 没等到应答，连接断了：{reason}")
            )


def _error_object(raw: Any) -> ErrorObject | None:
    """把帧里的 `error` 读成 `ErrorObject`；读不出来就 None。

    读不出来也要往上抛 —— 一个连 `code` 都没有的错误对象，调用方按 `code`
    分流的那套逻辑在它身上全都不成立，不如原样抖出去。
    """
    try:
        return ErrorObject.model_validate(raw)
    except ValidationError:
        return None


def _stderr_log(text: str) -> None:
    print(text, file=sys.stderr)


def _as_id(request_id: Any) -> str:
    """帧里的 id 归一化成 `_pending` 的 key。

    JSON-RPC 的 id 可以是数字也可以是字符串（`RequestId` 就是这么定义的），
    两种都得能用同一条路配对回来 —— 所以按字符串存，不猜类型。

    存和取**必须**都过这里：dict 不帮你转换 `1` 和 `"1"`，键写成数字、查用字符串，
    配不上是静悄悄的 —— 那个 `await` 会永远挂着。
    """
    return str(request_id)
