# joyczl-agent

Joy 的 Python 客户端。它本身不含任何逻辑 —— 把 `joy app-server` 拉起来，
用 stdin/stdout 跟它说 JSON-RPC。

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
        # 这条的应答里只有 turnId —— 正文是上面那条通知给的，别去应答上找。
        await client.request(
            Methods.TURN_START,
            TurnStartParams(sessionId="python:demo", message="你好", stream=True),
        )


asyncio.run(main())
```

## 装它

```bash
pip install joyczl-agent          # 只要客户端
pip install "joyczl-agent[bin]"   # 连 joy 二进制一起（分平台的单独一个包）
```

`joy` 二进制按这个顺序找，跟 TypeScript 侧同一套：

1. `$JOY_BIN`
2. 从本文件往上找到仓库根的 `joy-rs/target/{debug,release}/joy`
3. `pip install "joyczl-agent[bin]"` 装进来的那个
4. `PATH` 里的 `joy`

`JoyClient.connect()` 会自己把 app-server 拉起来，`close()`（或 `async with`
出去的时候）关掉它的 stdin，等它自己退。

## 出错的时候

服务端回来的错一律是 `JoyError`。`.code` 是 `Codes` 里那个数字 —— 那是 JSON-RPC
错误码，不是 HTTP 状态码，别拿去跟 4xx 比。要分流就用谓词：

```python
from joyczl_agent import JoyError

try:
    await client.request(Methods.TURN_START, TurnStartParams(message="你好"))
except JoyError as error:
    if error.is_provider:
        # 模型没配好、或者调模型失败：该请用户去配 key，不是他调错了。
        print("模型还没配好：", error)
    elif error.is_not_implemented:
        # 协议里有、这个阶段还没做（`model/list` 现在就是这种）。
        print("这个能力还没做：", error)
    elif error.is_invalid_params:
        # 参数不对 —— 这是调用方的 bug，该炸就炸。
        raise
```

`str(error)` 是服务端那句人话，可以直接给用户看。

## 通知怎么判别

用 `n.type` 这个字符串。那些名字好看的具名通知类（`TextDeltaNotification` 等）
**故意没导出**，因为拿去 `isinstance` 会是 False：JSON Schema 里联合的变体是内联
写开的，生成物里于是有**两份**同样的类型 —— 具名一份、内联一份，不是同一个类。
导出它们等于引诱你踩这个坑。等 Rust 侧把 schema 改成 `allOf: [$ref]` 就合一了，
那时再放开。

## 类型不是手写的

`generated/v2.py` 是 63 个命名的 pydantic 模型（外加 13 个联合内联出来的
`ServerNotificationN`），`generated/consts.py` 是 `Methods` 与 `Codes` 两个枚举 ——
都由 Rust 的 `joyczl-protocol` 生成：

```bash
just write-app-server-schema --python
```

所以 `Methods.MEMORY_REMEMBER` 背后的 `"memory/remember"` 这个字符串，在 Rust、
TypeScript、Python 里是同一处定义的。改协议的名字不需要记得来改这里，
`just test-protocol` 会在漂移时红。

`protocol.py` 里那份导出清单是**手写**的，这是被 Python 逼的：静态类型跟不上算
出来的再导出，`__all__` 一旦是推导出来的，mypy 就看不见 `from joyczl_agent import
Methods` 了。清单落后于生成物时，`tests/test_client.py` 会红。

## 自己验一遍

```bash
just py-check          # mypy + pytest
just smoke-sdk-python  # 真起 app-server 走一遍
```
