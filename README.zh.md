[English](README.md) | 简体中文

# Joyczl-agent

> 本地优先的个人助手：Rust 独占逻辑与状态，TypeScript 负责浏览器与聊天
> 网关，Python 只留一个薄 SDK。状态、推理与工具全部运行在用户自己的机器
> 上。项目代号 **Joy**，助手人格也叫 Joy。

## 设计原则

1. **Rust 独占逻辑与状态。** agent 循环、记忆、工具与 state.db 全在
   Rust；crate 按职责切碎（目标 <500 行/模块），不设 `joy-core` 大包。
2. **协议只有一份定义。** `joyczl-protocol` 是跨语言契约的唯一事实来源：
   类型直接生成 TypeScript 与 Python 绑定，生成物 check-in。
3. **客户端只做翻译。** CLI、dashboard、聊天网关、Python SDK 都是
   `joy app-server`（JSON-RPC over stdio）的客户端；state.db 只在一个
   进程里打开。

## 快速开始

```bash
cd joy-rs && cargo build
JOY_PROVIDER=deepseek DEEPSEEK_API_KEY=… ./target/debug/joy       # 终端对话
JOY_PROVIDER=deepseek DEEPSEEK_API_KEY=… ./target/debug/joy dashboard   # → http://localhost:7777
```

原始协议打一轮：

```bash
ANTHROPIC_API_KEY=sk-… JOY_HOME=/tmp/joy-demo ./target/debug/joy app-server <<'JSONRPC'
{"jsonrpc":"2.0","id":1,"method":"turn/start","params":{"message":"记住 alex 喜欢早上的会议"}}
JSONRPC
```

应答之前先来一串通知：`turnStarted → gateDecided → textDelta* →
toolStarted → toolCompleted* → turnCompleted`。`turnCompleted.meta` 记录
检索门判定、工具耗时、实际作答的模型与 token 用量——随对话落库。

## Joy 能做什么

- **对话**：终端、网页驾驶舱，或聊天网关（Telegram、Discord、微信、
  飞书——见 [docs/gateways.zh.md](docs/gateways.zh.md)）
- **记忆**：语义事实（FTS5、支持中文）、带日期的情景、过程 Skills——
  检索门保证只在相关时才翻记忆（[docs/architecture.zh.md](docs/architecture.zh.md)）
- **行动**：11 个内置工具外加 MCP 服务器
  （[docs/configuration.zh.md](docs/configuration.zh.md)）
- **图式规划**：triage 前门让闲聊便宜地得到快答；gather 工作流写晨报
  （[docs/architecture.zh.md](docs/architecture.zh.md)）
- **被检验**：确定性 eval 把守每次发布；judge 给回答质量打分
  （[docs/testing.zh.md](docs/testing.zh.md)）
- **被观察**：每轮落一条 trace 与一行用量
  （[docs/architecture.zh.md](docs/architecture.zh.md)）

### 内置工具

| 工具 | 用途 |
|---|---|
| `save_note` / `forget_note` / `search_memory` / `list_memory` / `manage_memory` | 记忆增删改查 |
| `create_event` / `list_events` | 日程（幂等，ICS + 可选 Apple Calendar 同步） |
| `send_message` | 消息草稿进 outbox——**从不真的发送** |
| `search_web` | DuckDuckGo HTML，设 `TAVILY_API_KEY` 升级 Tavily |
| `create_skill` | 把约定的工作流存成过程记忆 |
| `current_time` | 本地时间（含星期与时区） |
| `run_command` | 执行 shell 命令——沙箱 + 放行表，默认关（`JOY_EXEC=1` 才开，见 [SECURITY.zh.md](SECURITY.zh.md)） |
| `delegate_task` | 把一件自成一体的活交给子代理——默认关（`JOY_DELEGATE=1` 才开），且它不能再派生 |

工具失败以文本回给模型——loop 绝不因一次工具失败而崩。

## 文档

| 文档 | 内容 |
|---|---|
| [docs/architecture.zh.md](docs/architecture.zh.md) | 技术架构：原则、crate 依赖图、turn 数据流、关键机制、设计决策 |
| [docs/protocol.zh.md](docs/protocol.zh.md) | 协议参考：13 个方法、通知序列、错误码、客户端 |
| [docs/configuration.zh.md](docs/configuration.zh.md) | 配置参考：全部 `JOY_*` 变量、网关变量、mcp.json、settings.json |
| [docs/gateways.zh.md](docs/gateways.zh.md) | Telegram / Discord / 微信 / 飞书的接法、平台怪癖与安全提示 |
| [docs/testing.zh.md](docs/testing.zh.md) | 四层质量体系与最新测试报告 |
| [docs/operations.zh.md](docs/operations.zh.md) | 运行手册：各种运行形态、备份、故障排查 |
| [docs/limitations.zh.md](docs/limitations.zh.md) | 已知边界清单：刻意的与未做的，每条都指出下一步该打开哪个文件 |
| [docs/internals/](docs/internals/README.md) | 实现原理教程：逐章讲透每个 crate 的算法、数据流与不变量 |
| [docs/skills.zh.md](docs/skills.zh.md) | 技能创作：格式、触发规则、安装与分发 |
| [CONTRIBUTING.zh.md](CONTRIBUTING.zh.md) | 贡献指南：工程纪律与提交规则 |
| [SECURITY.zh.md](SECURITY.zh.md) | 安全模型：凭证、能力边界、OAuth |
| [CHANGELOG.zh.md](CHANGELOG.zh.md) | 更新日志 |

## 路线图

| 阶段 | 内容 |
|---|---|
| P0 ✅ | 协议层 + 生成管线（63 个类型） |
| P1 ✅ | state（SQLite+FTS5）、app-server（JSON-RPC over stdio）、`joy` CLI |
| P2 ✅ | config / provider（12 家，含 Ollama）/ tools / loop / memory（门 + 提炼） |
| P2b ✅ | provider 流式、MCP、图 + triage 前门 |
| P3 ✅ | 驾驶舱，Telegram / Discord / 微信 / 飞书网关（后三者未连过真平台） |
| P4 ✅ | 终端 REPL、turn/interrupt、config/write + model/list、完整工具集、Skills、trace/usage、gather、MCP OAuth、evals + gate + judge |
| P5 ✅ | 本地推理（Ollama，不需要 key）、`joy mcp serve`（把记忆暴露成 MCP 服务器）、Homebrew formula + Dockerfile |
| P6 ✅ | `JOY_EXEC` 三道闸门的沙箱执行（硬拒名单 + 放行表 + seatbelt/bwrap）、上下文压缩（滚动摘要）、声明式定时任务（`joy schedule`） |
| P7 ✅ | 混合检索（`JOY_EMBEDDINGS`，RRF）、临时陈述过滤、技能更新（`joy skill update`） |
| P8 ✅ | 启动期配置校验、工具参数 schema 校验、循环护栏、token 预算、沙箱且默认断网的执行、可见的重试、超长输出落盘、子代理、交互式批准、技能策略字段、记忆类别 |
| 下一步 | 分发：PyPI wheel + npm shim，均未发布 |

## 命名

| 用途 | 名字 |
|---|---|
| 仓库 / PyPI / crates.io | `joyczl-agent` |
| Rust crate 前缀 | `joyczl-` |
| CLI 二进制 | `joy` |
| 助手人格 | Joy |
| 环境变量 | `JOY_*` |
| 状态目录 | `.joy/` |

[English](README.md) | 简体中文 · Code is MIT.
