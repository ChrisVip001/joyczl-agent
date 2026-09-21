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

### 把 Joy 的记忆给别的 agent 用

反方向也有：`joy mcp serve` 让 Joy 自己成为一台 stdio 的 MCP 服务器，暴露
五个记忆工具（`memory_search`、`memory_remember`、`memory_forget`、
`memory_list`、`memory_episodes`），这台机器上的其他 agent 因此读写同一份
事实。**只暴露记忆**——不暴露任何「替别人动手」的能力。

```json
{"mcpServers": {"joy-memory": {
  "command": "joy",
  "args": ["mcp", "serve"],
  "env": {"JOY_HOME": "/Users/you/.joy"}
}}}
```

任何 MCP 客户端（Claude Code、codex……）配上这一段就与 Joy 共享长期记忆。
服务器用同一个 `open()` 开同一个 `state.db`（WAL + busy_timeout，多进程本来
就能共存）；别的 agent 写进来的事实标着 `source: mcp`，来源始终可查。

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

## 打包

```bash
docker build -t joy .
docker run --rm -p 7777:7777 -v joy-state:/home/joy/.joy \
  -e ANTHROPIC_API_KEY=… joy                 # 驾驶舱
docker run --rm -it -v joy-state:/home/joy/.joy \
  -e ANTHROPIC_API_KEY=… joy app-server      # stdio 上的协议
```

全部状态都在 `JOY_HOME` 下（镜像里是 `/home/joy/.joy`），一个 volume 就能带着走。
镜像里装了 `bubblewrap`——Linux 沙箱后端要的就是它，没有它 `run_command`
会拒绝执行任何命令（这是设计，不是故障）。Homebrew formula 模板在
`packaging/homebrew/joy.rb`（用 `cargo install` 从源码构建；发布 tarball 的
sha256 自己填）。

## 定时任务

`joy schedule` 是常驻进程，按五字段 cron（`*`、`*/步长`、`a-b`、`a,b`）触发
声明式任务。两种声明方式：

* 技能的 frontmatter 里带一行 schedule：

  ```markdown
  ---
  name: weekly-review
  description: 汇总这一周并起草周一简报
  schedule: 0 8 * * 1
  ---
  ```

* `<home>/schedules.json` 里的一条：

  ```json
  {"jobs": [{"name": "standup", "cron": "0 9 * * 1-5", "prompt": "今天有什么安排？"}]}
  ```

每次触发都在自己的会话里跑完整一轮（`schedule:<任务名>`），所以每条任务带着
自己的历史与滚动摘要；结果写进 `<home>/outbox/schedule-<名>-<时刻>.md`，
而不是往聊天里塞。同一分钟最多触发一次，任务串行执行，技能改动不用重启就
生效。交给 launchd/systemd 常驻 —— 或者干脆不用它，用系统 cron 直接调
`joy gather`。

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
