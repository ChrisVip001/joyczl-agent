# 回放评测（`joy eval --replay`）

回放用例回答的是一个不同的问题：**这次运行还会不会产生同一串事件**。确定性用例问的是「这个行为还对吗」，
回放用例问的是「批准、逐段输出、断线重连、压缩这些只在时间线里存在的东西，顺序还对吗」。

## 格式

一个文件一行 JSON（`#` 开头是注释，与 `evals/deterministic/` 同一套读法），字段：

| 字段 | 说明 |
| --- | --- |
| `id` / `description` | 用例标识与一句话说明（表里会打印） |
| `schemaVersion` | **必填**，当前为 `1`。缺失或对不上的文件会被当场拒绝 —— 老的录制文件不能猜着读 |
| `settings` | 与确定性用例同一个 `settings` 覆盖（`history_turns`、`exec_enabled`、`approval` …） |
| `files` | 跑之前写进 home 的夹具文件（MCP 假服务器、文章正文等） |
| `prereq` | `python3` / `sandbox` 之类前置；缺了是**跳过**，不是失败 |
| `recorded.turns[]` | 录下来的对话：`say`（人说的话）、`gate`（检索门脚本）、`replies[]`（模型应答，按调用顺序）、`approval`（人怎么回答审批）、`cwd` |
| `expect` | 与确定性用例**同一套断言**，外加 `events` |

`expect.events` 是**必填**且非空：一条不声明事件的「回放」只是在重复确定性用例，加载时就会被拒。

## 事件序列怎么匹配

按**有序子序列**匹配，不要求逐字相等：

- 期望 `["turnStarted", "toolStarted", "toolCompleted", "turnCompleted"]` 时，中间夹多少 `textDelta` / `toolDelta` 都不影响；
- 每项既能写事件名（`toolStarted`），也能写限定名（`toolStarted:run_command`、`toolCompleted:run_command:error`）；
- 失败信息会指出「第几个没出现」并打印实际序列。

一个形状上的注意点：`toolStarted` 是**边跑边发**的，而 `toolCompleted` 在收尾时按结果统一发 ——
所以一轮里有两个工具时，事件序读起来是「先两个 toolStarted，再两个 toolCompleted」。写期望值时
按这个顺序写（`continuation.jsonl` 就是这么排的），不要按调用先后交替写。

为什么不精确匹配：一次流式回答会吐出几十个 `textDelta`，精确匹配会让「多吐一个 delta」变成红灯 —— 那是噪声，
不是回归。真正要钉住的是「谁在谁之前」。

## 现在有哪几条

| 用例 | 钉住的东西 |
| --- | --- |
| `compaction.jsonl` | 窗口与 token 门**同时**命中：老轮次折进摘要，原文不再进 prompt |
| `approval.jsonl` | 没人回答 → 超时按拒绝算，这一轮照常收尾（`approvalRequested` 之后仍有 `turnCompleted`） |
| `exec.jsonl` | 命令输出**逐段可见**：`toolStarted → toolDelta → toolCompleted`（P0-3 的唯一端到端断言） |
| `mcp.jsonl` | MCP 断线 → 重建传输 → 重放握手 → 重试一次，工具真的跑通 |
| `continuation.jsonl` | 步数用尽但待办没做完 → **自动续一片**继续做（不是断崖），并证明续跑带上了刚才的步骤（`last_prompt_contains` 只可能来自前一段的消息）；次数用尽后如实收尾 |

## 加一条新用例

1. 在 `evals/replay/` 里挑一个语义相近的文件，追加一行；
2. 先写 `expect.events`（想钉住什么顺序），再写 `recorded.turns`；
3. 跑 `just eval-replay`，失败信息会打印实际事件序列 —— 用它校正期望值；
4. 通过后 `just check`（回放已经是门禁的一部分）。

## 为什么不做「真录制」

抓一次真实运行的 SSE 存盘再回放，需要录制端、脱敏、版本迁移三件套，收益是「能回放线上事故」。当前要的是
「边界可回归」：脚本化应答 + 事件序列达到同一目的，成本差一个量级。等真需要复盘线上事故时再加录制端，
`ReplayCase` 的读取路径不用动（它只是「能变成 Scenario 的壳」）。
