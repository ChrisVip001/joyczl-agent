"""Joy 的 Python 客户端。

把 `joy app-server` 拉起来，用它的 stdin/stdout 说 JSON-RPC —— 没有网络、没有
端口、没有常驻服务要管。

```python
import asyncio

from joyczl_agent import JoyClient, Methods, TurnStartParams


async def main() -> None:
    async with await JoyClient.connect() as client:

        def on_notification(n) -> None:
            # 判别式是 `type` 这个字段，跟 TS 侧 `n.type === "textDelta"` 一个写法。
            if n.type == "textDelta":
                print(n.delta, end="", flush=True)
            elif n.type == "turnCompleted":
                print("\n" + n.reply)

        client.on_notification(on_notification)
        # 这条的应答里只有 turnId —— 正文是上面那条通知给的。
        await client.request(
            Methods.TURN_START,
            TurnStartParams(sessionId="python:demo", message="你好", stream=True),
        )


asyncio.run(main())
```

类型不是手写的：`generated/v2.py` 和 `generated/consts.py` 都由 Rust 的
`joyczl-protocol` 生成（`just write-app-server-schema --python`）。所以
`Methods.MEMORY_REMEMBER` 背后的 `"memory/remember"` 这个字符串，在 Rust、
TypeScript、Python 里是同一处定义的。
"""

from __future__ import annotations

from .client import JoyClient, JoyError
from .protocol import *  # noqa: F403  ← `protocol` 自己定义了 `__all__`，出不了圈
from .protocol import __all__ as _protocol_exports
from .transport import StdioTransport, Transport, find_joy_binary

__all__ = [
    "JoyClient",
    "JoyError",
    "StdioTransport",
    "Transport",
    "find_joy_binary",
    *_protocol_exports,
]
