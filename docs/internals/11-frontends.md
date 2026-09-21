# ⑪ 前端与 SDK：`joy-ts` 与 `sdk/python`

Joy 的逻辑全在 Rust。前端与 SDK 做的是同一件事：**用另一种语言说 JSON-RPC**。
这一章讲它们各自怎么起进程、怎么分帧、以及各自不同的取舍。

目录：`joy-ts/`（`packages/{client,dashboard,gateway}`）、`sdk/python/`、
`sdk/python-bin/`。

## 11.1 共同的形状：typing 来自生成物

三方（Rust / TS / Python）之间只有一条类型通路：

```
joy-rs/joyczl-protocol/src/protocol/v2.rs      ← 唯一来源（①）
  ├─ schema/typescript/v2/*.ts         ← joy-ts 的 client/protocol.ts 直接 import
  └─ schema/json/v2.json               ← datamodel-codegen → SDK 的 generated/v2.py
```

于是每个包里都有一个**唯一指向生成物的文件**（TS 的 `protocol.ts`、Python 的
`protocol.py`），其它文件一律从它引。这样「改协议」只需要动 Rust + 重新生成，
而「忘了同步」会被 CI 的漂移检查挡住（⑫）。

值得一提的**分岔**：TS 侧对通知**刻意不做运行时校验**（形状由 Rust 保证，省一层
开销）；Python 侧则用 pydantic 校验一遍（因为校验规则也是同源生成的，白拿）。
两种都对，但各自要写清理由——不然下一个人会以为其中一个是漏了。

## 11.2 `packages/client`：TS 客户端

### 起进程（`transport.ts`）

`StdioTransport` 用 `child_process.spawn` 起 `joy app-server`，三件事：

- `readline.createInterface` 拆 stdout 成行（过滤空行后逐行交给 handler）；
- stderr 全部转给 `onLog`（**日志不能混进协议流**，⑨.6 的服务端侧规矩在客户端
  这一侧也成立）；
- `error` / `close` → `#shutdown`（把所有 pending 请求 reject 掉并给出原因）。

`close()` **只 `child.stdin.end()`，不 kill**：服务端看到 stdin 到头会写完通道里
剩余的帧再退——这是 ⑨.6 里那个 `drop(tx)` + 等 writer 的直接后果。杀进程会丢帧。

**找二进制**（`findJoyBinary`）：`$JOY_BIN` → 从本文件目录向上逐级找
`joy-rs/target/{debug,release}/joy` → `"joy"`（PATH）。开发期零配置，部署期可覆盖。

### 收发（`client.ts`）

```ts
#pending: Map<string, Pending>        // key 是 id 归一化后的字符串
request(method, params)               // 配 id → 写进 pending → 写一行 JSON
#handleLine(line)                     // JSON.parse → 有 method 是通知 / 有 error 是错误 / 否则配对 pending
#rejectAll(reason)                    // "{method} 没等到应答，连接断了：{reason}"
```

`RequestId` 可能是数字也可能是字符串（① 里 `untagged` 的设计），所以两端的
pending 表都按**归一化字符串**做 key——同一份协议在两种语言里都这么处理。

`JoyError` 带 `code` 与 `data`，并提供 `isProvider` / `isNotImplemented` /
`isInvalidParams` 三个判断——调用方（网关）据此给出人话（见 11.4）。

## 11.3 `packages/dashboard`：驾驶舱

**后端在 Rust 里**（`joyczl-ops`），不在这个包。这个包只有前端：

- `main.ts`：`/api/data` 拿首屏，`/api/session?…` 翻历史，`talk()` 发一轮并**逐帧
  渲染**；`POLL_MS = 15000` 定时刷新。
- `api.ts` 的 `talk()` 是个**异步生成器**：手读 `fetch` 的 body，按 `\n\n` 切帧、
  取 `data:` 载荷逐条 yield 出去；`finally` 里 `reader.cancel()` 收尾。
- `render.ts` 全是纯函数（`renderOverview` / `renderMemory` / `messageRow` /
  `metaChips` / `turnFooter`…），且**文本一律 `textContent`**（防 XSS——记忆里的
  内容是用户数据，不能当 HTML）。

Rust 侧（`joyczl-ops`）做的是「HTTP ↔ JSON-RPC 的翻译」：

| HTTP | 转到 |
|---|---|
| `GET /api/data` | `config/read` + `session/list{limit:20}` + `memory/list{limit:30}` + `memory/list-episodes{limit:20}` 合成一个 `DashboardData` |
| `GET /api/session` | `session/messages` |
| `POST /api/turn` | `turn/start`，应答改用 **SSE** 推通知流 |

SSE 那两个细节值得记：

- **必须先 `app.subscribe()` 再发 `turn/start`**，否则开头几个通知会漏掉；
- `forward` 用 `tokio::select!` 同时等「请求的应答」与「广播的通知」，
  按 `sessionId` 认下第一帧、之后按 `turnId` 过滤（`consolidationCompleted`
  与图事件不带 turnId，只在认下这轮之后才转）。

`AppServer::spawn` 起的是**自己**（`current_exe()`）加 `app-server` 参数，
`JOY_HOME` 用同一个——所以驾驶舱不会另开一个状态目录。

## 11.4 `packages/gateway`：四个平台

四个平台**接入方式完全不同**，这一章的价值在于对照：

| 平台 | 接入 | 白名单 | 切分上限 | 特殊之处 |
|---|---|---|---|---|
| Telegram | **长轮询** `getUpdates`（不是 webhook） | 数字 id 或 `@用户名` | 4096 | 退避封顶 30s；会话按**聊天**分（`telegram:{chatId}`） |
| Discord | **WebSocket**（HEARTBEAT + IDENTIFY） | 白名单 | 2000 | 频道里必须被 @；`MESSAGE_CONTENT` 是特权 intent |
| 微信 | **HTTP 回调服务器**（唯一需公网） | OpenID | —— | 明文/兼容/安全三模式；4000ms 内被动回复，否则先回 `success` 再走客服消息 |
| 飞书 | **长连接 WebSocket + 私有 protobuf** | open_id | 30000 | 手写 `pbbp2` 编解码；数据帧**先 ACK 再处理**（平台要求 3 秒内） |

四个平台共用 `bridge.ts（JoyBridge）`，它解决的是「平台无关的那些麻烦」：

- **会话即身份**：`sessionIdFor(conversation) = conversation`——无状态，
  `telegram:12345` 既是用户也是会话；
- **每会话一条尾巴**（`#tails`）：同一会话的消息排队执行，不并发；
- **`stream: false`**：聊天平台要一句完整的话，不要流式碎片；
- **`turnStarted` → `#inFlight` 的认领机制**：请求返回的 `turnId` 与通知里的
  `turnId` 配对（`#awaitingTurnId` → `#inFlight` → `#completed`），这样即使
  通知先于应答到达也不会乱；
- **`explain(error)`**：把 `JoyError` 翻译成人话（provider 错误 → 「模型那边没接
  上」；`notImplemented` → 「这个能力还没做」）——用户看的是聊天窗口，不是错误码。

`splitMessage(text, limit)` 只有一份实现：优先在换行处切（且换行位置要过半），
否则硬切，切掉后去掉开头的换行。四个平台的差异只是传入的上限不同。

**Discord 为什么不实现 RESUME**（代码注释里写得很清楚）：断线重连走「重新
IDENTIFY」，所以断的那几秒里说的话会丢；要补上得记住 `session_id` 与
`resume_gateway_url`——那会是这条链上唯一需要持久状态的地方，所以先不做。
代价是明确的：重启 Discord 网关会漏消息，重启 Telegram 网关不会。

## 11.5 `sdk/python`：Python 客户端

与 TS 客户端同构（`_pending` / 通知 handler / `_reject_all`），差异集中在传输与
类型：

**传输**（`transport.py`）：`asyncio.create_subprocess_exec` 起 `app-server`，
三个 task 分别泵 stdout、stderr 与 `wait()`。两个细节：

- `start()` 与构造函数分开，为的是**先把 handler 注册完再起泵**（消竞态）。
- `close()` 只 `stdin.close()`，然后 `await asyncio.gather(*tasks)` ——
  Python 的 asyncio 不像 Node 会把任务吊着，**必须显式等它退完**，否则最后几帧
  会在进程退出时消失。

`find_joy_binary()` 的顺序比 TS 多一跳：`$JOY_BIN` → 向上找
`joy-rs/target/{debug,release}/joy` → **`joyczl_agent_bin.binary()`**（pip 包里
自带的二进制）→ `shutil.which("joy")`。

**类型与「四条防线测试」**（`tests/test_client.py`，用内存假传输，不起真进程）：

1. 每个方法都有应答配对（`Methods` 与 `_RESPONSES` 对称）；
2. 每个方法类型都能从包里取到；
3. **12 个具名通知类故意不导出**（它们与联合类型里的内联副本不是同一个类，
   `isinstance(n, TextDeltaNotification)` 会是 False）——这条测试防的是「有人
   好心把它们导出去」；
4. 形状对不上的通知只记日志不抛（一条坏通知不该打死客户端）。

`test_every_method_has_a_response_pairing` 这类**自检型测试**值得抄：它不测行为，
测的是「本端与协议没有漂移」。

## 11.6 `sdk/python-bin`：为什么二进制单独成包

二进制是**分平台**的，客户端是**纯 Python** 的。混在一个包里，`py3-none-any`
轮子会被某个架构的 ELF 绑死；拆开之后：

- `joyczl-agent` 永远纯 Python（`joyczl-agent-bin` 是可选的 extra）；
- `joyczl-agent-bin` 用 hatch 的构建钩子（`hatch_build.py`）在打包时找二进制
  （`$JOY_BIN` → 向上找 `joy-rs/target/{release,debug}/joy`），找到就把轮子标签
  改成 `py3-none-{平台}` 并把二进制塞进去；
- **找不到也不报错**：退回纯 Python 轮子并打警告（`just build-python-bin` 负责在
  发布入口守「发出去的轮子必须有二进制」）。

仓库里的 `bin/` 目录**永远是空的**（`.gitkeep` 之外都忽略），因为二进制不该进
版本库——这是「一个二进制、一个状态目录」在打包链上的延续。
