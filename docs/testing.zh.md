# 测试策略与测试报告

[English](testing.md) | 简体中文

Joy 的质量体系分四层，从便宜到昂贵；**确定性 eval 100% 通过是发版闸门**
（release gate），一条失败就不发布。

## 分层

| 层 | 位置 | 形态 | 数量（最近一次全量） |
|---|---|---|---|
| 单元测试 | joy-rs 各 crate 内 | 进程内、离线、确定性 | 201 通过 / 15 套件 / 0 失败 |
| 协议生成 | scripts/write_schema.py --check | 生成物漂移检查 | 0 漂移 |
| 前端 | joy-ts（node --test + tsc） | 99 通过 / 0 失败 | mypy：Python SDK 8 文件 0 问题 |
| Python SDK | sdk/python（pytest + mypy） | 13 通过 / 0 失败 | |
| 端到端冒烟 | scripts/smoke*.sh | 真 app-server / 真网关 / 真驾驶舱 / 真 SDK | 4 套全通过 |
| 确定性 eval | evals/deterministic/*.jsonl | scripted 模型驱动同一 run_turn | 19/19 通过，gate exit 0 |
| judge | evals/judge/*.jsonl | 真模型答 + 裁判打分（0-10） | 出分不拦发版 |

## 单元测试分布（按 crate）

| crate | 用例 | 覆盖要点 |
|---|---|---|
| joyczl-state | 17 | SQL 契约、FTS5 检索（含 CJK 与乱码输入）、日历幂等、记忆后端 conformance |
| joyczl-tools | 19 | 工具行为与输出文案（落点如实）、create_event 幂等、SKILL.md 校验、参数错误文本化，执行的三道闸门，以及沙箱**真的**拦住了允许目录之外的写入 |
| joyczl-provider | 17 | 两种 wire format 转换、SSE 解析、429 限流、模型元数据，本地推理（不需要 key 的 provider） |
| joyczl-loop | 10 | 护栏双出口、工具往返、流式增量顺序、打断收兵 |
| joyczl-graph | 24 | 波次执行、路由、碰撞检测、on_error 排空、triage/gather 全路径 |
| joyczl-mcp | 30 | 传输帧、握手、工具注册、OAuth 全流程（本地假授权服务器）、token 存储 0600，以及它对外暴露的记忆服务器（`joy mcp serve`） |
| joyczl-memory | 33 | 检索门失败开放、consolidation（含临时陈述过滤）、Skills 触发/重扫、MEMORY.md 镜像、压缩水位线与兜底、RRF 融合、技能安装/更新 |
| joyczl-app-server | 19 | 协议分发、interrupt、config/write 持久化、model/list、trace/usage 落盘 |
| joyczl-cli | 7 | cron 语义、同一分钟只触发一次、两种来源装载任务、一次真触发落进 outbox |
| joyczl-config | 10 | 环境变量语义、补丁合并/落盘/清空 |
| joyczl-protocol | 7 | RPC 信封序列化（导出测试按需运行） |
| joyczl-ops | 8 | HTTP 翻译层 |

## 测试报告（2026-09-21 全量运行）

| 套件 | 结果 |
|---|---|
| cargo test --workspace（15 个二进制套件） | **201 通过 / 0 失败** |
| clippy --workspace --all-targets -D warnings | 0 告警 |
| cargo fmt --check | 通过 |
| 生成物漂移检查 | 与 Rust 协议定义一致 |
| joy-ts typecheck + node --test | 类型通过；**99 通过 / 0 失败** |
| Python SDK mypy + pytest | mypy 0 问题；**13 通过 / 0 失败** |
| scripts/smoke.sh（app-server 端到端） | 通过 |
| scripts/smoke-gateway.sh（四平台网关打桩链路） | 通过 |
| scripts/smoke-dashboard.sh（真驾驶舱 HTTP/SSE） | 通过 |
| scripts/smoke-sdk-python.sh（SDK 子进程生命周期） | 通过 |
| joy eval（确定性 eval，release gate） | **19/19，exit 0**（沙箱那条在没有可用沙箱的机器上如实计为跳过） |
| joy judge（deepseek-v4-pro 实测） | 2 用例出分；裁判捕获一次"声称已存但未调用 save_note"的虚报（0/10） |

## 确定性 eval 的断言能力

场景为 JSONL（一行一个用例），支持：多轮脚本、脚本化 gate 与模型应答
（text / tool_use）、回复包含/排除断言、**工具调用有序序列**、**工具输出
必须包含**（区分"调了"与"跑通"）、prompt 包含/排除（harness 给模型看了
什么）、consolidation 计数、interrupt 时序、config/write 热生效、
`prereq` 依赖缺失跳过。所有判定离线、可重复。

## 如何复现

```bash
just check            # fmt + schema 漂移 + ts-check + py-check + clippy + test + eval
just eval             # 仅确定性 eval（exit 0 = 可发布）
just judge            # judge 出分（需 API key）
just smoke            # app-server 端到端
just smoke-gateway / smoke-dashboard / smoke-sdk-python
```

失败处置约定：单元测试与确定性 eval 失败**阻塞合并**；judge 分数仅记录
趋势；smoke 失败阻塞发布。
