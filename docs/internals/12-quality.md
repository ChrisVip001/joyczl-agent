# ⑫ 质量体系：怎么证明它还能用

这一层没有产品功能，但它决定了前面十一章写的那些「失败开放」「不许裸跑」是不是
**真的**。四层：单元测试 → 确定性 eval → judge → 冒烟，越往上越贵、越接近真实用法。

目录：`joy-rs/joyczl-eval/`、`evals/`、`scripts/smoke*.sh`、`.github/workflows/ci.yml`。

## 12.1 四层各管什么

| 层 | 位置 | 形态 | 最新全量 |
|---|---|---|---|
| 单元测试 | 各 crate 的 `*_tests.rs` | 进程内、离线、确定性 | 202 通过 / 15 套件 |
| 确定性 eval | `evals/deterministic/*.jsonl` | scripted 模型驱动**真的** `run_turn` | 19/19，exit 0 |
| judge | `evals/judge/*.jsonl` | 真模型作答 + 裁判打分（0-10） | 出分，不拦发版 |
| 冒烟 | `scripts/smoke*.sh` | 真进程、真 stdio、真 HTTP | 4 套全通 |

分层的意义是**成本与确定性成反比**：单元测试毫秒级且完全确定；eval 仍是确定性的
（模型是假的），但跑的是真实的 `run_turn` 与真实 state.db；judge 不确定、要花钱，
所以只记趋势；冒烟最接近真实用法，但慢且需要环境。

## 12.2 确定性 eval：把「一轮对话」变成可断言的用例

### 场景格式（`scenario.rs`）

一个用例是一行 JSON（JSONL），字段全可选：

```json
{"id":"remember","description":"存事实：save_note 按序发出",
 "gate":{"retrieve":false,"query":"","reason":"note"},
 "responses":[{"tool_use":{"name":"save_note","args":{"subject":"a","content":"b"}}},
              {"text":"记好了。"}],
 "expect":{"tools_used":["save_note"],"reply_contains":["记好了"],
           "tool_output_contains":["已记住"]}}
```

| 字段 | 用途 |
|---|---|
| `message` / `gate` / `responses` | 单轮便捷写法 |
| `turns[]` | 多轮：每轮有自己的 `message` / `session_id` / `gate` / `responses` / `apply_patch` / `interrupt_after_deltas` |
| `settings` | 用例级旋钮覆盖：`history_turns`、`consolidate_every`、`retrieval_top_k`、`max_iterations`、`graph_workflows`、`exec_enabled`、`exec_allow` |
| `files` | 开跑前写进 home 的文件（内容里可用 `{{home}}` 占位） |
| `prereq` | `"python3"` / `"sandbox"`：前置不满足就**跳过**（计为跳过，绝不计为通过） |
| `expect` | 断言集合，见下 |

`responses` 里两种元素：`{"text":"…"}` 与
`{"tool_use":{"name":"…","args":{…}}}`。

**脚本队列的顺序 = 真实调用顺序**：每轮先放一条 gate 应答，再放这一轮的模型应答。
所以「第二轮在门之后、loop 之前还夹了一次压缩摘要调用」这种细节，在用例里就是
把摘要的应答排在第二条——**用例本身就是一份调用顺序的文档**（`compaction.jsonl`
的注释把这件事写明了）。

### 断言（`score`）

| 断言 | 含义 |
|---|---|
| `reply_contains` / `reply_not_contains` | 最终回复包含/不包含 |
| `tools_used` | **有序**的工具名序列；空数组 = 不检查 |
| `tool_output_contains` | 任一工具的输出包含片段——区分「调用了」与「真的成功了」 |
| `gate` | 检索门的决定 |
| `iterations` | 迭代次数 |
| `consolidation_new_facts` | 这一轮提炼出几条事实 |
| `interrupted` | 是否被打断 |
| `prompt_contains` / `prompt_not_contains` | **全部提示词**拼起来里是否有某片段 |
| `last_prompt_contains` / `last_prompt_not_contains` | 只看**最后一条**提示词 |

最后两类是这套体系里最有用的：它断言的是「**我们到底给模型看了什么**」。
滑窗是否真的滑出、摘要是否替换了原文、技能正文是否只在命中时才进 prompt——
全都靠它证明。

### 怎么跑的（`lib.rs`）

```
逐场景：
  1. prereq 检查（python3 / sandbox）→ 不满足就 skipped
  2. tempdir 当 home → 写 scenario.files → open_server(&settings)
  3. 装 RecordingProvider：内层是 Mock（按脚本弹），外层记录每个 CreateRequest
  4. 逐 turn：EventSink::channel() → spawn run_turn → 边收帧边做两件事：
       - interrupt_after_deltas 命中就調 server.interrupt_turn(turn_id)
       - 记 tool outputs、reply、iterations、gate、consolidation 计数
  5. apply_patch 存在则走**真协议** config/write（然后重新装回 mock，
     因为 config/write 会重解析 provider）
  6. score(expect, output) → 通过 ✓ / 失败 ✗ 逐条列原因
汇总 → 写 eval_report.json 与 eval_runs.jsonl → 退出码 = 失败数
```

三个设计选择：

- **用真的 `run_turn`**，不是「模拟一遍流程」。所以它测的是真正的接线：门、技能、
  滑窗、压缩、落库、通知顺序全都在里面。
- **假 key + `install_provider`**：`api_key: Some("eval-dummy")` 让 `boot` 的
  `resolve` 成功（否则连 Server 都装不起来），随后 provider 被换成 mock。
- **`interrupt_after_deltas` 用真实打断路径**（`server.interrupt_turn`），
  而不是直接调 loop 的取消——它测的是「协议方法 → 注册表 → 令牌 → loop」整条链。

### 19 条用例覆盖了什么

| 文件 | 钉住的东西 |
|---|---|
| `greeting` / `remember` / `recall` | 闲聊不查记忆、存事实按序发工具、问过去会检索 |
| `multi-turn` | 滑窗（第 4 轮只看得见最近两轮，第 1 轮已被折进摘要）、工具活动折叠防重复、会话隔离 |
| `consolidation` | 到点提炼、条数正确 |
| `interrupt` | 流式中途打断：半截回复落库且 `meta.interrupted` |
| `config-write` | 热生效（下一轮 `max_iterations=1` 立即卡护栏） |
| `graph-triage` | 图开着时的 quick/full 分流与失败开放 |
| `mcp` | 起假 MCP 服务器，工具注入与调用 |
| `compaction` | 摘要替换被挤出的轮次；摘要失败退化为确定性摘录（**原文不丢**） |
| `exec` | 默认关（工具不存在）、空放行表拒绝、硬拒名单压过 `*`、放行后**真在沙箱里跑**（`prereq: sandbox`） |

## 12.3 judge：真模型 + 裁判

`run_judge` 用**真是从环境来的** provider（`server_from_env`），没 key 就整组跳过
并打印「judge 需要 API key…整组跳过 —— 不拦发版」，返回 0。

流程：每个场景跑一轮拿回复 → 交给裁判模型按 `RUBRIC` 打分（0-10 + 理由）→
`parse_score` 从回复里抠 JSON → 打印 `{id} → {score}/10 {reason}` → 写报告。

**裁判必须不是被考的模型**（默认用小模型），且**分数只记趋势不拦发版**——真模型
的输出不稳定，用它当门是自欺欺人。历史战绩里它抓到过一次「声称已存但 `save_note`
never ran」的虚报（0/10），这就是这一层的价值。

## 12.4 四条冒烟：真进程

| 脚本 | 起什么 | 证明什么 |
|---|---|---|
| `smoke.sh` | 真 `joy app-server` + 假 SSE 服务器（`fake_sse_server.py`） | 协议 + 传输 + state 这条链通；含图路径与失败开放 |
| `smoke-gateway.sh` | 真 app-server + 四个网关的打桩链路 | 网关的会话/白名单/切分逻辑 |
| `smoke-dashboard.sh` | 真 app-server + 真 HTTP/SSE | 驾驶舱那条 HTTP→JSON-RPC 翻译 |
| `smoke-sdk-python.sh` | 真 app-server + 真 SDK | SDK 的子进程生命周期（起、说、退） |

它们**必须用真二进制**，因为测的正是「进程边界」——eval 用进程内调用绕过了
stdio 分帧与 writer 任务，那些地方出问题只有冒烟能发现。`smoke.sh` 已在 CI 的
release gate 步骤里跑（`bash ../scripts/smoke.sh`）。

## 12.5 CI：五道门

`.github/workflows/ci.yml` 五个 job：

| job | 跑什么 |
|---|---|
| Rust (fmt / clippy / test) | `cargo fmt --check`、`clippy --all-targets -D warnings`、`cargo test --workspace` |
| Protocol artifacts (drift check) | `python3 scripts/write_schema.py --check`（① 的漂移检查） |
| TypeScript | `npm install` → `npm run typecheck` → `npm test` |
| Python SDK | `uv sync` → `uv run mypy` → `uv run pytest` |
| Release gate | `cargo build -p joyczl-cli` → `joy eval evals/deterministic` → `bash ../scripts/smoke.sh` |

两个刻意的选择：

- **`clippy -D warnings`**：警告即错误。项目里没有「先留着以后修」的告警。
- **release gate 把 eval 与冒烟放在一起**：evals 证明「逻辑对」，冒烟证明「进程
  边界对」。这两件事各自出过问题（一次是测试本身平台相关、一次是 Rust↔前端的缝），
  所以都不当可选项。

`just check` 是本地版：`fmt-check + schema drift + ts-check + py-check + eval`，
再加 `clippy` 与 `cargo test`。**提交前跑它**——本轮工作里 CI 连红六次就是没跑全
导致的（本地全绿但 CI 红，因为一个测试断言依赖了本机有沙箱）。

## 12.6 写一条新用例的步骤

```
1. 想清楚要钉住哪个行为（一条用例一个行为，别塞两件事）
2. 按真实调用顺序排脚本队列：每轮 [gate, …本轮模型应答]，压缩/提炼的调用按时序插
3. 选断言：行为 → reply_*/tools_used；接线 → prompt_contains；安全边界 →
   tool_output_contains（区分「调了」与「真的做到」）
4. 需要机器上未必有的东西 → 加 prereq，让它在别的机器上诚实跳过
5. 跑 `joy eval evals/deterministic/你的文件.jsonl`（一行一个用例，多行 JSON 会报错）
6. 对着失败原因改：`跑不起来：…` 是接线问题，`✗` 加原因是断言问题
```

## 12.7 这套体系的已知缺口（`docs/limitations.md`）

- embedding 那条路**没有对着真 embedding 服务的端到端测试**：融合与降级有单测，
  HTTP 调用本身没有。
- judge 依赖真 key，CI 里不跑。
- 网关与驾驶舱的冒烟不在 CI 里跑（需要 npm / 前端构建产物）。

**这一层的不变量**：确定性 eval 与冒烟是发版门槛；judge 只记趋势；需要环境的东西
声明 `prereq` 并诚实跳过；一条用例只钉一个行为。
