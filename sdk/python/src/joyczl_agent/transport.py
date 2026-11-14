"""A frame's boundary, and how to pull up a `joy app-server`.

The app-server's stdout carries nothing but "one JSON object per line"; stderr carries the
human words (MCP cannot connect, the graph will not assemble…). So the transport has three
jobs only: write a line, split a line, and hand the other two streams out untouched.

An abstract base class so a test can drop a fake in — assembling frames and the pairing that
goes with it (id pairing, notification fan-out, settling pending requests on exit) is the
client's own logic, and starting a process to test it would be the wrong way round.
"""

from __future__ import annotations

import asyncio
import os
import shutil
from abc import ABC, abstractmethod
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path

#: The app-server sits under this subcommand of `joy`.
APP_SERVER_COMMAND = "app-server"

LineHandler = Callable[[str], None]


class Transport(ABC):
    """A frame's boundary."""

    @abstractmethod
    def write(self, line: str) -> None:
        """Write one line (without the terminator)."""

    @abstractmethod
    def on_line(self, handler: LineHandler) -> None:
        """One line arrived (without the terminator)."""

    @abstractmethod
    def on_log(self, handler: LineHandler) -> None:
        """One line on stderr, passed through unparsed."""

    @abstractmethod
    def on_close(self, handler: LineHandler) -> None:
        """The process exited / would not start / the connection dropped."""

    @abstractmethod
    async def start(self) -> None:
        """Start reading frames.

        Kept apart from building the object to kill a race: **register the handlers, then start
        the pump**, and there is no "the first few frames arrived before anyone was
        listening". On the TS side the process is started right in the constructor, and it
        only gets away with it because every registration happens in the same tick.
        """

    @abstractmethod
    async def close(self) -> None:
        """A civil close: close stdin, so the server finishes the queued frames and exits."""


class StdioTransport(Transport):
    """Start a real `joy app-server` and talk over its stdin/stdout.

    This is the entry shape of every client: one child process, two pipes, no network, no port,
    no temp directory to clean up. The server dies and the pipes break, and the client knows.
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
            # The most common failure, and asyncio's own words
            # ("No such file or directory") say neither which path it looked for nor where
            # to put one.
            raise FileNotFoundError(
                f"cannot find `joy`: {self._command}\n"
                "Run `cargo build` once (in joy-rs/), or point JOY_BIN at it."
            ) from None

        stdout = self._proc.stdout
        stderr = self._proc.stderr
        assert stdout is not None and stderr is not None  # PIPE is what makes them exist

        self._tasks = [
            asyncio.create_task(self._pump(stdout, self._line_handlers)),
            asyncio.create_task(self._pump(stderr, self._log_handlers)),
            asyncio.create_task(self._wait()),
        ]

    def write(self, line: str) -> None:
        proc = self._proc
        if self._closed or proc is None or proc.stdin is None:
            return
        # No drain wait: a frame is a few tens of KB at most, and the child keeps reading. If it
        # ever backed up, somebody is pushing bulk data through the protocol, and that
        # belongs there, not a whole back-pressure stack padded in here.
        proc.stdin.write(f"{line}\n".encode())

    async def close(self) -> None:
        proc = self._proc
        if self._closed or proc is None:
            return
        # Close stdin only, do not kill: the end of stdin is the server's signal to finish the
        # frames left in the channel and then exit (see the end of stdio.rs). A kill here
        # would eat those last few frames.
        if proc.stdin is not None:
            proc.stdin.close()
        # Then wait for it to really be gone. TS can skip this — Node's event loop stays up
        # until the child exits; asyncio does not, and `asyncio.run()` cancels every
        # unfinished task the moment it returns, so those last frames would vanish too.
        await asyncio.gather(*self._tasks, return_exceptions=True)

    @property
    def closed(self) -> bool:
        return self._closed

    async def _pump(self, stream: asyncio.StreamReader, handlers: list[LineHandler]) -> None:
        # `readline()` handles half-lines itself: a hand-rolled buffer joining strings always
        # ends up missing the cut across a chunk. It splits on \n only, so \r is
        # collected here.
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
        self._shutdown(f"exit (code={code})")

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
    """The one installed by `pip install "joyczl-agent[bin]"`.

    Not installed means None — it is an optional dependency, and invisible is the normal case,
    not an error.
    """
    try:
        # An optional dependency, and not being there is the norm. mypy not finding it states
        # something we already know.
        from joyczl_agent_bin import binary  # type: ignore[import-not-found]
    except ImportError:
        return None
    candidate = binary()
    return str(candidate) if candidate.is_file() else None


def find_joy_binary() -> str:
    """Find the `joy` binary.

    The order: `$JOY_BIN` → walk up from this file to the repo root's
    `joy-rs/target/{debug,release}` → the bin package pip installed → hand it to PATH.

    Walking up rather than hard-coding "five directories up": where the source sits inside
    the package moves, and with a fixed depth baked in, one directory move turns "cannot
    find the binary" into an inexplicable FileNotFoundError.
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
