# 更新日志

[English](CHANGELOG.md) | 简体中文

## 0.5.0 — 本地、沙箱、定时

- 本地推理：`ollama` provider —— 不要 key、不要网络，对话不出这台机器。
  LM Studio / vLLM 用 `JOY_BASE_URL` + `JOY_MODEL` 指过去
- `joy mcp serve`：Joy 成为一台 MCP 服务器，暴露五个记忆工具，这台机器上的
  其他 agent 共享同一份事实（**只暴露记忆**，不暴露任何「替别人动手」的能力）
- 沙箱执行：`run_command` 挂在 `JOY_EXEC` 后面，默认关，三道闸门 ——
  不可配置的硬拒名单、放行表（空 = 全部拒绝）、macOS `sandbox-exec` /
  Linux `bubblewrap`（写权限限制在工作目录、Joy 的 home 与临时目录）。
  沙箱不可用就拒绝执行，绝不在沙箱之外跑
- 上下文压缩：被滑窗挤出去的轮次折进按会话的滚动摘要（存 state.db，只往前滚、
  不重算全史）；摘要模型罢工时退化为确定性摘录
- `joy schedule`：定时任务可声明在技能 frontmatter（`schedule: 0 8 * * 1-5`）
  或 `schedules.json` 里，五字段 cron，同一分钟最多触发一次，结果写进 outbox
- 混合检索（`JOY_EMBEDDINGS`，关键词与向量两条腿按名次 RRF 融合）+
  `joy memory reindex` 给开关打开之前的事实补向量
- consolidation 过滤临时陈述，不把它们当成事实存档
- `joy skill update`：按索引更新 —— 先校验、再暂存、备份、原子替换，从不降级
- 分发：Dockerfile（多阶段、非 root、装了 bubblewrap）与 Homebrew formula 模板
- `docs/limitations.zh.md`：已知边界清单（刻意的与未做的），每条都指出下一步
  该打开哪个文件

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
