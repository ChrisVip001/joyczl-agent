# 更新日志

[English](CHANGELOG.md) | 简体中文

## 0.4.0 — 功能面补齐

- 协议：`turn/interrupt`（取消令牌 + 竞速收兵）、`config/write`
  （settings.json 持久化 + 热生效）、`model/list` 全部实现；
  `ToolStarted` 通知、`TurnMeta.usage` / `meta.interrupted` 入库
- CLI：终端 REPL（`joy` 裸跑）、`joy gather` 晨报、`joy mcp login`、
  `joy skill export/install/list`
- 工具：`search_web`、`create_event`/`list_events`（幂等 + ICS +
  Apple Calendar 可选同步）、`send_message`（outbox 草稿）、
  `manage_memory`、`create_skill`
- 记忆：Skills 过程记忆（渐进披露 + 关键词触发）、MEMORY.md 每轮镜像
- 观测：trace 落 `traces/<日期>.jsonl`、usage 账本 `usage.jsonl`
- 图：gather 晨报工作流（四路并行、只提议不行动、失败开放）
- MCP：浏览器 OAuth（发现 / 动态注册 / PKCE / 回调 / token 落盘 /
  刷新），stdio + HTTP 双传输
- 评测：`joyczl-eval`——确定性 eval（13 场景）+ release gate（`just
  check` 尾部）+ judge
- 文档：architecture / configuration / protocol / testing /
  operations / skills / CONTRIBUTING / SECURITY

## 0.3.0 — 驾驶舱与网关

- `joyczl-ops`：axum + SSE 驾驶舱后端；`@joy/dashboard` 前端
- 网关：Telegram、Discord（WebSocket + 心跳重连）、微信（三种加解密
  模式 + 4 秒竞速 + 客服消息）、飞书（长连接 + pbbp2 帧 + 分片重组）
- 会话：`session/*` 协议与历史翻页

## 0.2.0 — loop 与编排

- `joyczl-provider`：11 家厂商、Anthropic/OpenAI 双 wire format、SSE 流式
- `joyczl-loop`：observe → reason → act → repeat，工具错误文本化
- `joyczl-memory`：检索门（失败开放）+ consolidation
- `joyczl-tools`：内置工具注册表；`joyczl-graph`：波次 DAG 引擎 +
  triage 前门；`joyczl-mcp`：stdio + HTTP 传输

## 0.1.0 — 地基

- `joyczl-protocol`：单一事实来源 + TS/JSON Schema/pydantic 生成管线
- `joyczl-state`：SQLite + FTS5(trigram) + 迁移
- `joyczl-app-server`：JSON-RPC over stdio；`joy` CLI
