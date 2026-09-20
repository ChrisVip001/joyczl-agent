# 运行手册

[English](operations.md) | 简体中文

日常运行的每一种形态、推荐的环境变量与常见故障。

## 终端对话

```bash
cd joy-rs && cargo build
JOY_PROVIDER=deepseek DEEPSEEK_API_KEY=… ./target/debug/joy
```

流式输出；`/memory [词]` 查记忆、`/sessions` 列会话、`/new` 新会话、
`/quit` 退出。中断：`Ctrl-C` 在终端里是退出进程——网页端才用
`turn/interrupt` 打断。

## 网页驾驶舱

```bash
cd joy-ts && npm run build --workspace @joy/dashboard   # 前端只需构建一次
JOY_HOME=$HOME/.joy ./target/debug/joy dashboard        # → http://localhost:7777
```

首屏展示配置 / 会话 / 记忆；聊天区实时呈现检索门判定、工具调用与流式
回复。端口被占自动 +1（最多 10 个）。

## MCP 外挂

写 `<home>/mcp.json`（stdio 或 HTTP 服务器），工具自动并入工具表。
远程服务器用 `"oauth": true` 时先 `joy mcp login <名>`；token 过期自动
刷新，没有 refresh_token 时重跑 login。

## 晨报（cron 友好）

```bash
JOY_GH_REPO=owner/repo joy gather
```

四路并行扫描 → 一次综合 → 摘要打印、草稿写进 `outbox/gather-<日期>.md`。
单个数据源失败变成报告里的 "unavailable"，不影响整体。

## Python SDK 嵌入

```python
import os
from joyczl_agent import JoyClient

async with await JoyClient.connect() as client:   # 拉起 joy app-server 子进程
    client.on_notification(lambda n: print(n.type))
    await client.request("turn/start", {"message": "你好", "stream": True})
```

环境变量由嵌入方进程决定（子进程继承）；`JOY_BIN` 可指定二进制。
打包：`just build-python-bin` 产出分平台 wheel。

## 数据与备份

`<home>/` 下值得备份的只有几样：`state.db`（记忆与对话，事实来源）、
`SOUL.md`（人格）、`skills/`（过程记忆）、`mcp-auth/`（OAuth token，
0600）。`traces/`、`usage.jsonl`、`MEMORY.md`、`outbox/` 是可再生成的
派生物。

## 故障排查

| 症状 | 处置 |
|---|---|
| `PROVIDER_ERROR (-32000)` 缺 key | 按 .env→环境变量链路补 key 后重启进程 |
| 401 / 403 | key 失效或地区不可达；换 key 或换 `JOY_BASE_URL` |
| MCP 工具不见了 | 看 stderr 警告：连不上 / 未登录（`joy mcp login`）/ mcp.json 坏 |
| 回答里 `[tools used: …]` 重复 | 属预期折叠标记，说明工具活动已入历史 |
| 想知道"它当时为什么这么答" | 打开 `traces/<日期>.jsonl` 对应 turnId 那一行 |
| 端口 7777 被占 | dashboard 自动 +1，启动日志里有实际端口 |
