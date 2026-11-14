# ⑪ 前端：`joy-ts`

Joy 的逻辑全在 Rust。前端与 SDK 做的是同一件事：**用另一种语言说 JSON-RPC**。
这一章讲它们各自怎么起进程、怎么分帧、以及各自不同的取舍。

目录：`joy-ts/`（`packages/{client,dashboard,gateway}`）。

## 11.1 共同的形状：typing 来自生成物

两侧（Rust / TS）之间只有一条类型通路：

```
joy-rs/joyczl-protocol/src/protocol/v2.rs      ← 唯一来源（①）
  ├─ schema/typescript/v2/*.ts         ← joy-ts 的 client/protocol.ts 直接 import
  └─ schema/json/v2.json               ← 给非 TS 调用方的机器可读副本
```

于是包里只有一个**指向生成物的文件**（TS 的 `protocol.ts`），其它文件一律从它引。
「改协议」只需要动 Rust + 重新生成；「忘了同步」会被 CI 的漂移检查挡住（⑫）。
非 TS 的调用方不必等一个官方 SDK —— 协议是 stdio 上的 JSON-RPC，几行代码就能说
（见 `docs/operations.md` 的示例）。

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
- `render.ts` 全是纯函数（`renderOverview` / `renderTaskCard` / `messageRow` /
  `metaChips` / `turnFooter`…），且**从不拼 HTML**：文字走 `textContent`，结构走
  `createElement`（防 XSS——模型回复和记忆内容都是外部输入，没有理由变成标记）。
- `markdown.ts` 把模型的回复画成 Markdown：**自己写的解析器 + 直接建 DOM**，不引
  第三方 parser，`innerHTML` 一次都不用（原始 HTML 按文本显示，图片不渲染，链接只
  放行 http/https/mailto 与站内路径）。流式期间保持纯文本——每帧追加一个文本节点，
  几千 token 的回复不会因为重排而抖动——一轮结束后一次性换成渲染结构。只作用于模型
  的 `.bubble.md`；用户自己说的话保持纯文本，不被重新解释。

Rust 侧（`joyczl-ops`）做的是「HTTP ↔ JSON-RPC 的翻译」：

| HTTP | 转到 |
|---|---|
| `GET /api/data` | `config/read` + `session/list{limit:20}` + `memory/list{limit:30}` + `memory/list-episodes{limit:20}` 合成一个 `DashboardData` |
| `GET /api/session` | `session/messages` |
| `POST /api/turn` | `turn/start`，应答改用 **SSE** 推通知流 |
| `POST /api/run` | `turn/start`，但**同步**等这一轮跑完，返回 `RunResponse`（任务 API；程序用的那一半） |
| `POST /api/hook/{name}` | 读 `<home>/webhooks.json` → 鉴权（`X-Joy-Secret` 或时间戳+HMAC 签名，重放 409）→ 限流（429）→ `turn/start`；配了 `callback` 就 202 + 后台跑完再推结果（入站 webhook；**在 token 层之外**） |
| `GET /api/model/routes` / `POST /api/model/routes/save` | `model/routes` / `model/routes/save`：档位路由表 |

SSE 那两个细节值得记：

- **必须先 `app.subscribe()` 再发 `turn/start`**，否则开头几个通知会漏掉；
- `forward` 用 `tokio::select!` 同时等「请求的应答」与「广播的通知」，
  按 `sessionId` 认下第一帧、之后按 `turnId` 过滤（`consolidationCompleted`
  与图事件不带 turnId，只在认下这轮之后才转）。

`AppServer::spawn` 起的是**自己**（`current_exe()`）加 `app-server` 参数，
`JOY_HOME` 用同一个——所以驾驶舱不会另开一个状态目录。

### 记忆中心：抽屉的第四个页签

三个二级分段，一次只显示一段（抽屉只有几百像素宽，三栏并排会挤成三条缝）：

| 分段 | 数据 | 交互 |
|---|---|---|
| 账本 | `GET /api/ledger/turns`（一行一轮） | 点开一行 → `GET /api/ledger/turn` 取该轮条目，就地展开 |
| 记忆库 | `GET /api/memory/index` + `GET /api/memory/page` | 目录 → 正文；页脚的 `ledger:…` chip 跳回账本那一轮 |
| 上下文 | `GET /api/context` | 六段分层条 + 已用/预算；到阈值出现一句提示；按钮打 `/api/compact` |

两条约束，都是踩过的坑：

- **打开就重读，不缓存。** 账本只增、bundle 会被后台 worker 改写、占用率属于上一轮 ——
  缓存下来就是把「过去」当成「现在」展示。
- **`must()` 的 id 必须与页面同时更新。** 页面和 bundle 版本错配会整屏白屏；`index.html` 带
  `main.js?v=<build>` 版本戳，`sw.js` 的 `VERSION` 每次构建递增，`smoke-dashboard.sh` 对着
  `must("…")` 的 id 列表逐条比对 —— 三处任缺一处，白屏就会回来。

「重新生成」故意不做成按钮：整页重生成是一次模型调用，它属于 `joy memory rebuild`，
不属于界面里一个顺手点下去的链接。

### 语言：中文是 key

界面先只有中文，加英文时没有引入符号化的 key（`sessions.empty.title`），而是**让中文自己
当 key**（gettext 的老办法）：

```ts
el("p", { text: t("还没有会话") })
t("已排队 —— 前面还有 {n} 条", { n: queued - 1 })
```

三个理由：

- **调用点可读。** `t("还没有会话")` 在代码里是什么，一眼就知道；`t("sessions.empty")` 要跳表。
- **不翻译是看得见的。** 忘了包 `t()` 的字符串就明晃晃地留在代码里，而不是被键名藏起来。
- **占位符跟着句子走。** `t("{n} 分钟前")` / `"… {n} min ago"` —— 词序可以两种语言各写各的。

代价是「键」不唯一时容易重复（同一个中文串在两处写两遍），所以词典本身就是去重表：同一条
中文只有一份英文，改一处两处都变。

**切换 = 刷新。** 每个字符串在绘制时烘进 DOM，页面由十几个函数拼出来；要让已有节点就地换语言，
得把「语言变了」的事件穿过整个 `main.ts`。刷新重跑那条已经会画的路径，代价是 100ms，收益是
只有一条代码路径。

**静态标记用属性**（`index.html`）：`data-i18n`（文本）、`data-i18n-title` /
`data-i18n-placeholder` / `data-i18n-aria`（属性 —— `placeholder` 和 `aria-label` 和标签一样
是被看见的）。`applyStatic()` 在 `boot()` 的第一行跑，早于任何绘制。

**默认跟随浏览器**：`localStorage.joy.lang` 有值就用它，否则 `navigator.language` 以 `zh`
开头取中文、其余取英文。

**刻意不翻的**：

- 模型输出、会话标题、任务标题、验收条目、停止原因 —— 这些是**数据**，不是界面文案；翻了
  就等于改写用户的东西。
- `SKILL.md` 骨架（新建技能时的模板）—— 它是写进文件的正文，所以按语言给两份常量，不进词典。
- `t()` 缺英文时回退中文并 `console.warn` 一次：宁可留一句中文，也不留一个空按钮。

`scripts/check_i18n.py`（`just check-i18n`，也在 `just check` 与 CI 里）守三条：**用了的必须
翻译过**、**翻译过的必须有人用**（否则是改完调用点忘了删）、**两边占位符名字必须一致**（不一致
时值会在一种语言里静默消失）。它比人眼可靠的地方在于：`t()` 回退中文不会抛错，界面只是多了
一句中文。

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
