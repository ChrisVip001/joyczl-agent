"""The Python client for Joy.

Pull up `joy app-server` and speak JSON-RPC over its stdin/stdout — no network, no port, no
long-running service to manage.

```python
import asyncio

from joyczl_agent import JoyClient, Methods, TurnStartParams


async def main() -> None:
    async with await JoyClient.connect() as client:

        def on_notification(n) -> None:
            # The discriminant is the `type` field, spelled the same as
            # `n.type === "textDelta"` on the TS side.
            if n.type == "textDelta":
                print(n.delta, end="", flush=True)
            elif n.type == "turnCompleted":
                print("\n" + n.reply)

        client.on_notification(on_notification)
        # The response to this one carries only turnId — the text came from the notification
        # above.
        await client.request(
            Methods.TURN_START,
            TurnStartParams(sessionId="python:demo", message="hello", stream=True),
        )


asyncio.run(main())
```

The types are not hand-written: `generated/v2.py` and `generated/consts.py` are both
generated from Rust's `joyczl-protocol` (`just write-app-server-schema --python`). So the
string `"memory/remember"` behind `Methods.MEMORY_REMEMBER` is defined in one place, across
Rust, TypeScript and Python.
"""

from __future__ import annotations

from .client import JoyClient, JoyError
from .protocol import *  # noqa: F403  ← `protocol` defines its own `__all__`, nothing
                          # escapes
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
