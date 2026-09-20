"""一帧的边界，和怎么把一个 `joy app-server` 拉起来。

app-server 的 stdout 上只有「一行一条 JSON」，stderr 上是人话（MCP 连不上、
图搭不起来……）。传输层的活因此只有三件：写一行、拆一行、把另外两路原样交出去。

抽成抽象基类是为了测试能塞一个假的进去 —— 帧的组装/对应关系（id 配对、通知分发、
退出时收敛挂着的请求）是客户端自己的逻辑，不该为了测它去起进程。
"""

from __future__ import annotations

import asyncio
import os
import shutil
from abc import ABC, abstractmethod
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path

#: app-server 就在 `joy` 的这个子命令下。
APP_SERVER_COMMAND = "app-server"

LineHandler = Callable[[str], None]


class Transport(ABC):
    """一帧的边界。"""

    @abstractmethod
    def write(self, line: str) -> None:
        """写一行（不含行尾）。"""

    @abstractmethod
    def on_line(self, handler: LineHandler) -> None:
        """收到一行（不含行尾）。"""

    @abstractmethod
    def on_log(self, handler: LineHandler) -> None:
        """stderr 上的一行，原样转出，不解析。"""

    @abstractmethod
    def on_close(self, handler: LineHandler) -> None:
        """进程退出 / 起不来 / 连接断开。"""

    @abstractmethod
    async def start(self) -> None:
        """开始读帧。

        跟建对象分开，是为了消掉一个竞态：**handler 注册完再起泵**，就不会有
        「头几帧已经到了、还没人接」这回事。TS 那边是在构造函数里直接起进程的，
        它靠「注册都发生在同一个 tick」侥幸躲过这一刀。
        """

    @abstractmethod
    async def close(self) -> None:
        """文明关闭：关掉 stdin，让服务端把排着的帧写完再退。"""


class StdioTransport(Transport):
    """起一个真的 `joy app-server`，用它的 stdin/stdout 说话。

    这是所有客户端的入口形状：一个孩子的两根管子，没有网络、没有端口、没有要
    清理的临时目录。服务端死了管子就断，客户端跟着知道。
    """

    def __init__(
        self,
        *,
        command: str | None = None,
        args: Sequence[str] | None = None,
        cwd: str | Path | None = None,
        env: Mapping[str, str] | None = None,
    ) -> None:
        self._command = command if command is not None else find_joy_binary()
        self._args = list(args) if args is not None else [APP_SERVER_COMMAND]
        self._cwd = str(cwd) if cwd is not None else None
        self._env = dict(env) if env is not None else None

        self._line_handlers: list[LineHandler] = []
        self._log_handlers: list[LineHandler] = []
        self._close_handlers: list[LineHandler] = []

        self._proc: asyncio.subprocess.Process | None = None
        self._tasks: list[asyncio.Task[None]] = []
        self._closed = False

    def on_line(self, handler: LineHandler) -> None:
        self._line_handlers.append(handler)

    def on_log(self, handler: LineHandler) -> None:
        self._log_handlers.append(handler)

    def on_close(self, handler: LineHandler) -> None:
        self._close_handlers.append(handler)

    async def start(self) -> None:
        try:
            self._proc = await asyncio.create_subprocess_exec(
                self._command,
                *self._args,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                cwd=self._cwd,
                env=self._env,
            )
        except FileNotFoundError:
            # 这是最常见的失败，而 asyncio 给的原话（"No such file or directory"）
            # 不会告诉人它找的是哪个路径，也不会说该往哪放。
            raise FileNotFoundError(
                f"找不到 `joy`：{self._command}\n"
                "先 `cargo build`（在 joy-rs/ 下）跑一次，或者用 JOY_BIN 指到它。"
            ) from None

        stdout = self._proc.stdout
        stderr = self._proc.stderr
        assert stdout is not None and stderr is not None  # PIPE 就是要了才有

        self._tasks = [
            asyncio.create_task(self._pump(stdout, self._line_handlers)),
            asyncio.create_task(self._pump(stderr, self._log_handlers)),
            asyncio.create_task(self._wait()),
        ]

    def write(self, line: str) -> None:
        proc = self._proc
        if self._closed or proc is None or proc.stdin is None:
            return
        # 不等 drain：一帧撑死几十 KB，而子进程一直在读。真要压满了，那是有人
        # 拿协议传大块数据了，该在那边解决，不该在这里垫一整套背压。
        proc.stdin.write(f"{line}\n".encode())

    async def close(self) -> None:
        proc = self._proc
        if self._closed or proc is None:
            return
        # 只关 stdin，不 kill：服务端看到 stdin 到头就会把通道里剩的帧写完再退
        # （见 stdio.rs 末尾）。这时候补一刀会把最后那几帧吃掉。
        if proc.stdin is not None:
            proc.stdin.close()
        # 然后等它真退。TS 那边可以不等 —— Node 的事件循环会一直挂到子进程退出；
        # asyncio 不会，`asyncio.run()` 一返回就把没跑完的任务全取消了，最后那几帧
        # 会跟着一起消失。
        await asyncio.gather(*self._tasks, return_exceptions=True)

    @property
    def closed(self) -> bool:
        return self._closed

    async def _pump(self, stream: asyncio.StreamReader, handlers: list[LineHandler]) -> None:
        # `readline()` 自己管住「半行」：手写缓冲拼字符串最后一定会漏掉跨 chunk
        # 的那一刀。它只按 \n 切，所以还要自己收掉 \r。
        while True:
            raw = await stream.readline()
            if not raw:
                return
            line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
            if not line.strip():
                continue
            for handler in list(handlers):
                handler(line)

    async def _wait(self) -> None:
        proc = self._proc
        if proc is None:
            return
        code = await proc.wait()
        self._shutdown(f"退出（code={code}）")

    def _shutdown(self, reason: str) -> None:
        if self._closed:
            return
        self._closed = True
        for handler in list(self._close_handlers):
            handler(reason)
        self._line_handlers.clear()
        self._log_handlers.clear()
        self._close_handlers.clear()


def _bundled_joy() -> str | None:
    """`pip install "joyczl-agent[bin]"` 装进来的那个。

    没装就返回 None —— 那是个可选依赖，看不见是常态，不是错。
    """
    try:
        # 可选依赖，没装是常态。mypy 查不到它是在说一件我们已经知道的事。
        from joyczl_agent_bin import binary  # type: ignore[import-not-found]
    except ImportError:
        return None
    candidate = binary()
    return str(candidate) if candidate.is_file() else None


def find_joy_binary() -> str:
    """找 `joy` 二进制。

    顺序：`$JOY_BIN` → 从本文件往上找到仓库根的 `joy-rs/target/{debug,release}`
    → pip 装进来的那个 bin 包 → 交给 PATH。

    往上走而不是写死「上五级目录」：源码在包里的位置是会动的，而写成固定层数
    之后，挪一次目录就会把「找不到二进制」变成一个莫名其妙的 FileNotFoundError。
    """
    override = os.environ.get("JOY_BIN")
    if override:
        return override

    for directory in Path(__file__).resolve().parents:
        for profile in ("debug", "release"):
            candidate = directory / "joy-rs" / "target" / profile / "joy"
            if candidate.is_file():
                return str(candidate)

    bundled = _bundled_joy()
    if bundled is not None:
        return bundled

    return shutil.which("joy") or "joy"
