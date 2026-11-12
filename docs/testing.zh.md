# 测试策略与测试报告

[English](testing.md) | 简体中文

Joy 的质量体系分四层，从便宜到昂贵；**确定性 eval 100% 通过是发版闸门**
（release gate），一条失败就不发布。

## 分层

| 层 | 位置 | 形态 | 数量（最近一次全量） |
|---|---|---|---|
| 单元测试 | joy-rs 各 crate 内 | 进程内、离线、确定性 | 603 通过 / 0 失败 |
| 覆盖率 | `cargo llvm-cov` / `coverage` | **趋势门**，不是目标 | Rust 行 65.9%（门 60）/ Python 63%（门 60） |
| 协议生成 | scripts/write_schema.py --check | 生成物漂移检查 | 0 漂移 |
| 前端 | joy-ts（node --test + tsc） | 110 通过 / 0 失败 | mypy：Python SDK 8 文件 0 问题 |
| Python SDK | sdk/python（pytest + mypy） | 13 通过 / 0 失败 | |
| 端到端冒烟 | scripts/smoke*.sh | 真 app-server / 真网关 / 真驾驶舱 / 真 SDK | 4 套全通过 |
| 确定性 eval | evals/deterministic/*.jsonl | scripted 模型驱动同一 run_turn | 66/66 通过，gate exit 0 |
| 回放 eval | evals/replay/*.jsonl | 录制对话 + **期望事件序列**（压缩/审批超时/增量输出/MCP 断线/步数用尽自动续片） | 5/5 通过，同样进 gate |
| 长任务基准 | evals/long/*.jsonl | 同一装置，问「自主任务有没有变强」 | 11/11 达成（含一条开着计划器的），中断恢复 3/3；**不拦发版** |
| judge | evals/judge/*.jsonl | 真模型答 + 裁判打分（0-10） | 10 例；出分不拦发版 |

## 单元测试分布（按 crate）

| crate | 用例 | 覆盖要点 |
|---|---|---|
| joyczl-state | 24 | SQL 契约、FTS5 检索（含 CJK 与乱码输入）、日历幂等、记忆后端 conformance、事实类别的归一化、提炼失败的退避、并发认领（两个 turn 同时到期只蒸馏一次） |
| joyczl-tools | 96 | 工具行为与输出文案（落点如实）、create_event 幂等、SKILL.md 校验、参数 schema 校验、执行的三道闸门与批准（含沙箱**真的**拦住了允许目录之外的写入）、超长输出落盘、子代理工具表里没有它自己；本轮新增：12 个 hook 事件的阻断/改写/超时两分法与信任哈希、待办清单三条硬边界、后台作业的环溢出与 owner 围栏、read_file/list_dir 的分页与重读短路、fetch_url 的 SSRF 拒绝与 HTML 转文本 |
| joyczl-provider | 24 | 两种 wire format 转换、SSE 解析、429 退避重试（真 HTTP）、token 估算、本地推理（不需要 key 的 provider） |
| joyczl-loop | 25 | 护栏双出口、工具往返、流式增量顺序、打断收兵 |
| joyczl-graph | 24 | 波次执行、路由、碰撞检测、on_error 排空、triage/gather 全路径 |
| joyczl-mcp | 33 | 传输帧、握手、工具注册、OAuth 全流程（本地假授权服务器）、token 存储 0600，以及它对外暴露的记忆服务器（`joy mcp serve`） |
| joyczl-memory | 41 | 检索门失败开放、consolidation（临时陈述过滤、类别、失败退避）、Skills 触发/重扫、显式 `$技能` 与依赖缺失、MEMORY.md 镜像、压缩水位线与兜底、RRF 融合、技能安装/更新 |
| joyczl-agent | 13 | 确定性内核：三条验收检查（存在/子串/成功调用计数）、工具自述与文件事实对账（含矛盾检出）、计划落账与失败降级、路径收敛（工作目录外/无目录一律拒绝） |
| joyczl-app-server | 104 | 协议派发、打断、config/write 持久化、model/list、trace/usage 落盘、批准等待表（含「先问再跑」的因果）、子代理；工具结果预算与实测 prefill 校准、子代理报告扫描与 `result_schema` 两阶段接受、生命周期钩子、待办注入、后台作业真进程（无沙箱则跳过）、目标循环的续轮/上限/判断器失败三态；以及任务引擎：无会话任务、假完成停在需要确认、真写入才通过验收、超预算停在花费前、一次执行一行台账、重启对账不重放、多轮步骤幂等键不撞、中断立即停止任务循环；以及会话门：同一会话第二轮被 `SESSION_BUSY` 挡在任何副作用之前、`steer` 在轮次边界生效、`follow-up` 排队重启不丢、空闲时调用明说没对象 |
| joyczl-config | 27 | 环境变量语义、启动期校验（越界即退出）、补丁合并/落盘/清空（**不再有模型/密钥字段**）；多套模型配置的读写、凭据后端（内存/文件，不碰真钥匙串）、切换与删除 |
| joyczl-cli | 7 | cron 语义、同一分钟只触发一次、两种来源装载任务、一次真触发落进 outbox |
| joyczl-protocol | 7 | RPC 信封序列化（导出测试按需运行） |
| joyczl-ops | 8 | HTTP 翻译层 |

## 测试报告（2026-09-23 全量运行）

| 套件 | 结果 |
|---|---|
| cargo test --workspace（15 个二进制套件） | **255 通过 / 0 失败** |
| clippy --workspace --all-targets -D warnings | 0 告警 |
| cargo fmt --check | 通过 |
| 生成物漂移检查 | 与 Rust 协议定义一致 |
| joy-ts typecheck + node --test | 类型通过；**99 通过 / 0 失败** |
| Python SDK mypy（strict）+ pytest | mypy 0 问题；**13 通过 / 0 失败**；覆盖率 63%（门 60） |
| scripts/smoke.sh（app-server 端到端） | 通过 |
| scripts/smoke-gateway.sh（四平台网关打桩链路） | 通过 |
| scripts/smoke-dashboard.sh（真驾驶舱 HTTP/SSE） | 通过 |
| scripts/smoke-sdk-python.sh（SDK 子进程生命周期） | 通过 |
| joy eval（确定性 eval，release gate） | **42/42，exit 0**（要沙箱的那几条在没有可用沙箱的机器上如实计为跳过） |
| joy judge（deepseek-v4-pro 实测） | 2 用例出分；裁判捕获一次"声称已存但未调用 save_note"的虚报（0/10） |

## 哪些东西 eval 断不了（以及怎么补的）

eval 跑的是「一条消息进、一条消息出」的同一套 `run_turn`，所以它能钉住工具往返、
通知顺序与最终答复。三类东西它钉不住，各有各的补法：

* **多轮之后才成立的状态**（待办清单在下一轮被重新注入）：写成 app-server 的端到端
  测试 —— 同一个 `Server` 上跑两轮，断言第二轮请求的 system prompt。
* **真实时间与真实进程**（后台作业、hook 的超时）：写成带真进程的测试，并且**没有
  可用沙箱就如实跳过**（不假装通过）。
* **需要人在场才能启动的东西**（目标循环）：eval 脚本改不了「这个人设了一个目标」，
  所以它的覆盖是 app-server 的端到端测试（判断器用一个脚本化的模型，续轮、`round-limit`
  与判断器失败三条路各一条）。

## 确定性 eval 的断言能力

场景为 JSONL（一行一个用例），支持：多轮脚本、脚本化 gate 与模型应答
（text / tool_use，多轮任务用 `{"gate": …}` 标出每一轮的门脚本）、回复包含/排除断言、
**工具调用有序序列**、**工具输出必须包含**（区分"调了"与"跑通"）、prompt 包含/排除
（harness 给模型看了什么）、consolidation 计数、interrupt 时序、config/write 热生效、
`prereq` 依赖缺失跳过。所有判定离线、可重复。

`prompt_contains` 读的是模型**真正看到**的东西：稳定前缀 + 每轮尾巴，再加上工具表的
名字与说明。两个新用例族就靠这一点成立 —— `deterministic/tool-policy.jsonl`（延迟加载
的工具 schema 不在请求里、`find_tools` 打开它、下一次请求就有了）与
`deterministic/prompt-shape.jsonl`（薄/全灵魂、目录在但正文不在）。场景级 settings 也
跟着长了对应的旋钮：`soul_mode`、`tool_policy`、`skill_index_tokens`、`skill_body_tokens`、
`tool_result_keep_recent`、`context_warn_ratio`。

## 回放 eval（`joy eval --replay`）

确定性 eval 问「这个行为还对吗」，回放 eval 问「**这次运行还会不会产生同一串事件**」。
`evals/replay/*.jsonl` 的录制格式是「人说的话 + 脚本化应答 + `expect.events`」，加载时被
转成同一个 `Scenario`，因此运行器与断言完全与确定性 eval 共用 —— 差别只有 loader 与那条
事件断言，不存在第二套框架。

事件按**有序子序列**匹配：`["turnStarted", "toolStarted:run_command", "toolDelta",
"toolCompleted:run_command:ok", "turnCompleted"]` 里的每项只要按顺序出现即可，中间多几个
`textDelta` / `toolDelta` 不算失败（精确匹配会让「多吐一个 delta」变成红灯，那是噪声）。
`expect.events` 为空的行在加载时就被拒绝：一条不声明事件的「回放」只是在重复确定性用例。

五条用例各钉一个边界：压缩（窗口与 token 门同时命中）、审批超时（没人回答 → 拒绝且能收尾）、
增量输出（`toolDelta` 必须在 `toolCompleted` 之前）、MCP 断线（重建传输 → 重放握手 → 重试一次）、
步数用尽但待办没写完（自动续一片，且续跑**带着**前一段的步骤 —— 一条只可能来自消息历史的
`last_prompt_contains` 就是这条的证词）。
也是发版闸门的一部分（`just check` 里含 `eval-replay`）。

## 长任务基准（P1）

`joy bench` 跑 `evals/long/*.jsonl`：同一套 scripted 装置，但问的是另一个问题 ——
**这次改动让自主任务变强了还是变弱了**。两种口径分开统计，混在一起会掩盖真正该看的东西：

* **任务成功率**：达到用例自己声明的终态（全部是 `done`，即验收按事实全过）的比例；
* **中断恢复率**：标了 `recovery` 的子集里，某一轮被真的打断、服务进程被真的重启（同一个
  home，对账跑过）之后，后续回合仍然把任务收尾的比例。

它**只记录、不拦发版**（失败也 exit 0）：基准的价值是趋势，把它当闸门只会逼出一堆
为了让数字好看而写的用例。报告落 `eval_report.json`，同时留一份
`eval_report_long.json`，免得基准运行覆盖掉确定性 eval 的结论；历史仍在
`eval_runs.jsonl`（带 `suite` 字段）。

用基准抓到的两个真实缺陷（已修，各有回归用例）：

* **多轮步骤幂等键相撞**：每轮都从 1 开始编号，`<turn_id>:<seq>` 会撞，第二轮的写入被
  静默吞掉，`toolsOk` 永远只数到一次；
* **中断后继续烧轮次**：被打断的那一轮验收不过 → 「接着试」又买一轮，用户的停止只在轮次
  用尽时才生效。

## 如何复现

```bash
just check            # fmt + schema 漂移 + ts-check + py-check + clippy + test + eval
just eval             # 仅确定性 eval（exit 0 = 可发布）
just bench            # 长任务基准（只记录，不拦发版）
just judge            # judge 出分（需 API key）
just smoke            # app-server 端到端
just smoke-gateway / smoke-dashboard / smoke-sdk-python
```

失败处置约定：单元测试与确定性 eval 失败**阻塞合并**；长任务基准与 judge 分数仅记录
趋势；smoke 失败阻塞发布。
