# AGENTS.md

[English](AGENTS.md) | 简体中文

Joy 是一个本地优先的个人助手：Rust 独占逻辑与状态，TypeScript 负责浏览器
与聊天网关，Python 只留一个薄 SDK。改 `joy-rs/` 前先读
[docs/architecture.md](docs/architecture.md)；本文件是 AI 编码助手与人类
贡献者共用的仓库工作规范。

## 仓库布局

```
joy-rs/
  joyczl-protocol/      跨语言契约，唯一事实来源（schema/ 生成物 check-in）
  joyczl-config/        JOY_* 环境变量，启动读一次
  joyczl-state/         SQLite + FTS5(trigram)；记忆后端契约与 conformance
  joyczl-provider/      12 家厂商（含本地 Ollama），两种 wire format + SSE
  joyczl-tools/         工具注册表 + 内置工具
  joyczl-loop/          agent 主循环（可打断）
  joyczl-graph/         波次 DAG 引擎 + triage / gather 工作流
  joyczl-mcp/           MCP 客户端（stdio/HTTP）+ 浏览器 OAuth + 记忆服务器（`joy mcp serve`）
  joyczl-memory/        检索门 + consolidation + Skills + MEMORY.md 镜像
  joyczl-app-server/    JSON-RPC over stdio，唯一持有 state.db
  joyczl-ops/           驾驶舱后端（axum + SSE）
  joyczl-cli/           joy 二进制（REPL / dashboard / gather / eval / mcp / skill）
  joyczl-eval/          确定性 eval + judge + release gate
joy-ts/                 @joy/client、@joy/gateway、@joy/dashboard
sdk/python/             Python 薄客户端（pydantic 模型为生成物）
evals/                  确定性 eval 与 judge 用例（JSONL）
docs/                   架构 / 配置 / 协议 / 测试 / 运行 / 技能 / 已知边界
                        internals/ —— 实现原理教程（每层一章） 文档
scripts/                schema 生成与 4 条端到端冒烟
```

## 命令

```sh
just check                    # 全部门禁：fmt + schema 漂移 + ts-check + py-check
                              # + clippy -D warnings + cargo test + 确定性 eval
just eval / just judge        # 确定性 eval（exit 0 = 可发布）/ judge 出分
just write-app-server-schema  # 改协议后必跑；just check-app-server-schema 为 CI 漂移检查
just smoke smoke-gateway smoke-dashboard smoke-sdk-python   # 端到端冒烟
```

## 不可违反的不变量

- **协议单一定义**：类型只写在 `joyczl-protocol/src/protocol/v2.rs`，
  改动后必须跑 `just write-app-server-schema` 并提交生成物；wire 字段
  camelCase、整数一律 `i32`、新类型登记进 `export.rs` 的清单。
- **工具错误是文本，不是异常**：`ToolRegistry::execute` 永不返回 Err。
- **失败开放必须配测试**：检索门、triage、gather 的失败路径与成功路径
  同等重要。
- **clippy 零告警**（`-D warnings`）；crate 目标 <500 行/模块。
- **配置只来自环境变量**；`config/write` 的持久化补丁走校验，非法值
  整体拒绝。
- **不新增重型依赖**：先确认工作区里没有等价物（URL 编解码、哈希等
  已有实现）。
- **凭证只走环境变量**：不进任何文件、协议载荷或日志；OAuth token 落
  `mcp-auth/`（0600）。
- **只提议，绝不行动**：gather 图保持无工具；`send_message` 永不真发。

## 测试

行为改动配测试：单元测试放 crate 内，跨模块组装行为放
`evals/deterministic/*.jsonl`（一行一个用例，支持多轮/prompt 断言/
打断/配置注入）。修 bug 先在测试里复现。`joy eval` 失败阻塞发布。

## 文档

用户可见的行为改动同步更新 README 与 `docs/` 对应分册；文档用中文，
一段一个事实，不叙述控制流。

## 文档双语政策

文档以**英文为主**（`*.md`），每份文件配有中文对译（`*.zh.md`）。改任何
一份文档时同步更新它的对译——成对提交、成对评审。术语以英文版为准。
