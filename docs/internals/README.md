# 实现原理教程（Internals）

这是一份**按模块讲透实现**的文档：每个 crate 做什么、怎么做、为什么这么做，
以及要改哪一段代码。它不是 API 参考（那个在 `docs/protocol.md` 与代码注释里），
而是「带你读懂这台机器怎么转」的路线。

面向的读者：能读 Rust，想改 Joy 的行为、加一个工具/一个 provider/一条工作流，
或者只是想搞清楚「一轮对话到底发生了什么」的人。

## 怎么读

**第一次读**：先看下面那段「10 分钟全链路」，再按 ①→⑫ 顺序读。每一章都可以
独立成立，但章节顺序就是依赖顺序（被依赖的层在前面）。

**带着任务读**（跳读表）：

| 你想做的事 | 读哪几章 |
|---|---|
| 加一个工具 | ⑥ 工具层（`Tool` 契约）+ ⑨ 服务端（工具表怎么装配） |
| 加一个模型厂商 | ④ provider 层（`PROVIDERS` 表 + `resolve`） |
| 加一条工作流/图 | ⑧ 图引擎（`run_graph` 的波次语义 + 注入闭包） |
| 改提示词/记忆行为 | ⑦ 记忆层（门、提炼、压缩、技能、检索） |
| 改协议（加方法/加字段） | ① 协议层（单一来源 + 生成管线）+ ⑨ 服务端（dispatch） |
| 加一个 CLI 子命令 | ⑩ 命令行 |
| 改前端/网关 | ⑪ 前端与 SDK |
| 加验收用例 | ⑫ 质量体系（eval 场景格式与断言） |

## 章节

| # | 章 | 内容 |
|---|---|---|
| ① | [协议层](01-protocol.md) | 63 个类型的单一来源、代码生成管线、JSON-RPC 信封、noop 宏为什么存在 |
| ② | [存储层](02-state.md) | state.db 的每一次 PRAGMA、5 个迁移、每张表的 SQL 契约、FTS5 trigram 与 LIKE 回退 |
| ③ | [配置层](03-config.md) | `Settings` 的只读一次模型、`SettingsPatch` 的三级叠加、`config/write` 的落盘顺序 |
| ④ | [provider 层](04-provider.md) | 12 家厂商表、`resolve` 的优先级链、两种 wire format 的纯函数翻译、SSE 解析、embedding |
| ⑤ | [agent 循环](05-loop.md) | `Turn` 的全部输入、双护栏、流式与非流式如何归一、打断的三个安全点 |
| ⑥ | [工具层](06-tools.md) | `Tool`/`ToolRegistry` 契约、9 个内置工具逐个讲、`run_command` 的三道闸门与沙箱 |
| ⑦ | [记忆层](07-memory.md) | 检索门、consolidation、上下文压缩的水位线、RRF 混合检索、技能渐进披露、技能安装 |
| ⑧ | [图引擎](08-graph.md) | 波次执行算法、写冲突检测、路由与 `on_error`、triage 前门、gather 晨报 |
| ⑨ | [服务端](09-app-server.md) | `Server` 装配、`run_turn` 17 步流水线、dispatch 的 13 个方法、stdio 并发模型、trace |
| ⑩ | [命令行](10-cli.md) | 8 个子命令、REPL 的流式渲染、`joy schedule` 的 cron、`joy gather` 的真实绑定 |
| ⑪ | [前端与 SDK](11-frontends.md) | TS 客户端/驾驶舱/四平台网关、Python SDK 与分平台二进制包 |
| ⑫ | [质量体系](12-quality.md) | eval 场景格式与断言、mock provider、judge、四条冒烟、CI 门 |

## 10 分钟全链路：`joy` 里敲一句话，发生了什么

以 `JOY_PROVIDER=deepseek`、默认配置、`joy` 裸跑进入 REPL 为例。

1. **启动**（⑩）：`main` 看到没有子命令 → 读环境变量得到 `Settings`（③）→
   `joyczl_app_server::open` 把 `<home>/settings.json` 里的补丁叠上去 →
   `state.db` 建/开、WAL、迁移跑到最新（②）→ `resolve()` 拿 provider 客户端（④）→
   `builtin_tools()` 组装工具表（内置 9 个 + 可能有的 exec + MCP）（⑥）→ 进 REPL。
2. **你敲的这句话**进 `chat_turn`：起一个 `EventSink` 通道，`run_turn` 跑在另一个
   任务里，REPL 同时开始消费通知（⑩）。
3. **`run_turn` 第一步**（⑨）：登记取消令牌（`turns[turn_id] = Interrupt`）并挂上
   `TurnGuard`（`Drop` 时摘表），发 `TurnStarted` 通知。
4. **前门 triage**（⑧，默认关）：开着时 `classify`（小模型判 quick/full，失败开放）
   与 `check_calendar`（读 `calendar.ics`）并排跑，汇到 `gather` 后由路由器决定走
   `quick_reply`（一次小模型直答）还是 `full_agent`（完整 loop）。
5. **完整路径**（⑦）：检索门问小模型「这条消息要不要翻记忆」（失败开放，
   `GateDecided` 通知立刻发出）→ 门说查就 `retrieve_context`（关键词 + 可选向量，
   见 ⑦）→ 扫技能 frontmatter 取**匹配上**的技能正文（渐进披露）→
   `load_history` 取最近 N 轮 + 把被窗口挤出去的老轮次折成滚动摘要。
6. **拼 system prompt**（⑨）：SOUL.md → 当前时间 → 模型自述 → 相关记忆 →
   命中的技能 → 早先对话摘要。
7. **loop**（⑤）：带当前工作记忆调一次模型（走 SSE 流式，每个 delta 既转发给
   REPL 也作为 `Text` 事件给观察者）→ 模型要工具就逐个执行（`ToolStarted` 先发，
   `Tool` 后发）→ 工具结果作为一轮 User 消息喂回去 → 再调模型，直到模型不再要工具
   （护栏 1）或到 `max_iterations`（护栏 2）。全程与取消令牌竞速（`turn/interrupt`）。
8. **收尾**（⑨）：`ToolCompleted` 通知补齐 → `fold_tool_activity` 把「用过什么工具」
   折进回复文本（防重复预订会议）→ 组装 `TurnMeta`（门/图/迭代/耗时/工具/模型/
   **usage**/interrupted）→ 落 `chat_log`（assistant 行带 meta）→ trace 与 usage
   各写一行 → 到点就跑 consolidation（⑦，提炼事实）→ 刷新 `MEMORY.md` 镜像 →
   发 `TurnCompleted` → **最后**才发 JSON-RPC 应答（保证客户端先收到全部通知）。
9. **REPL 侧**（⑩）：`TextDelta` 逐个 `print!`，工具与门的小字插在中间，
   `TurnCompleted` 收尾打印「— 模型 · 轮数 · 耗时 · token」。

再加一句要点的：**任何一层失败都不许让整轮对话崩**——门失败开放、图失败回退
loop、摘要失败退化摘录、向量失败只用关键词、trace 写失败只喊 stderr、工具错误
变成文本。这条「失败开放/降级」的主线在每一章里都会再出现一次，认得它就认得了
这个项目的性格。

## 与其它文档的分工

| 文档 | 管什么 |
|---|---|
| `docs/architecture.md` | 高层次：原则、crate 依赖图、关键机制清单、设计决策 |
| **docs/internals/**（本文） | 低层次：每个模块的算法、数据流、不变量、代码位置 |
| `docs/protocol.md` | 协议参考：方法、通知序列、错误码（对外契约） |
| `docs/configuration.md` | 每个 `JOY_*` 变量的语义 |
| `docs/limitations.md` | 已知边界：刻意的取舍与未做的工作 |
| 代码注释 | 具体某一段为什么这么写（本教程会反复指过去） |
