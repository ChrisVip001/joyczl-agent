import io


def patch(path, pairs):
    d = io.open(path, encoding="utf-8").read()
    for old, new in pairs:
        assert old in d, f"{path}: 缺锚点 {old[:70]}"
        d = d.replace(old, new, 1)
    io.open(path, "w", encoding="utf-8").write(d)


# 配置文档
patch(
    "docs/configuration.zh.md",
    [
        (
            "| `JOY_LLM_RETRIES` | 0 – 5 |",
            "| `JOY_LLM_RETRIES` | 0 – 5 |\n| `JOY_GOAL_MAX_ROUNDS` | 1 – 20 |",
        ),
        (
            "| `JOY_LLM_RETRIES` | `2` |",
            "| `JOY_LLM_RETRIES` | `2` |\n"
            "| `JOY_GOAL_MAX_ROUNDS` | `5` | 一个目标最多自动续几轮。超了就置 `round-limit` 并如实说明没完成 —— 不伪装完成、也不清掉目标 |",
        ),
    ],
)
patch(
    "docs/configuration.md",
    [
        (
            "| `JOY_LLM_RETRIES` | 0 – 5 |",
            "| `JOY_LLM_RETRIES` | 0 – 5 |\n| `JOY_GOAL_MAX_ROUNDS` | 1 – 20 |",
        ),
        (
            "| `JOY_LLM_RETRIES` | `2` |",
            "| `JOY_LLM_RETRIES` | `2` |\n"
            "| `JOY_GOAL_MAX_ROUNDS` | `5` | how many rounds a goal may continue for; on overrun the status is `round-limit` and the miss is reported honestly |",
        ),
    ],
)
patch(
    ".env.example",
    [
        (
            "#JOY_LLM_RETRIES=2",
            "#JOY_LLM_RETRIES=2\n# 目标最多自动续几轮（/goal 设的目标；超了如实说没完成）。\n#JOY_GOAL_MAX_ROUNDS=5",
        )
    ],
)

# limitations
patch(
    "docs/limitations.zh.md",
    [
        (
            "## 记忆",
            """* **刻意** —— 目标循环的判断器**只看对话**：便宜模型、没有工具、没有检索。
  它要回答的是「这段对话说明目标达成了吗」，多看别的只会让它开始替模型干活。
  也正因为如此，它对「做完了吗」的判断可能不准 —— 所以它只能**续轮**，不能
  替人决定放行任何东西。
* **刻意** —— 目标只能由人设/改/清（`/goal` 或 `goal/set`）：模型没有这条路径。
  到达终止态（达成/做不到/超轮次/判断器挂了）之后循环不会自己再跑，等下一句人的话。

## 记忆""",
        )
    ],
)
patch(
    "docs/limitations.md",
    [
        (
            "## Memory",
            """* **Deliberate** — The goal loop's judge **sees only the conversation**: a
  cheap model, no tools, no retrieval. Its question is "does this conversation show
  the goal met", and giving it more would just have it start doing the work. It
  follows that its verdict can be wrong — which is why it may only *continue a
  round*, never let anything past a permission check.
* **Deliberate** — Goals are set, changed and cleared by humans only (`/goal` or
  `goal/set`): the model has no path to them. After a terminal outcome (satisfied /
  impossible / round-limit / judge failure) the loop stops by itself and waits for a
  human.

## Memory""",
        )
    ],
)

# protocol 文档
patch(
    "docs/protocol.zh.md",
    [
        (
            "`approval/respond`",
            """`goal/set`（设/清一个目标）、通知 `GoalRound`（每判一次发一条）：

```
you>  /goal 让测试全绿
       → goal/set  {sessionId, condition: "让测试全绿"}
       ← {active: true, condition: "让测试全绿", roundsUsed: 0, maxRounds: 5}
… 一轮跑完，判断器说不算达成 …
       ← GoalRound {turnId, round: 1, maxRounds: 5, status: "continuing", reason: "还有两条红"}
… 又跑一轮 …
       ← GoalRound {turnId, round: 2, maxRounds: 5, status: "satisfied", reason: "全绿了"}
```

`TurnMeta.goalStatus` / `goalRounds` 把出口与轮次也带进这一轮的元数据。

`approval/respond`""",
        )
    ],
)
patch(
    "docs/protocol.md",
    [
        (
            "`approval/respond`",
            """`goal/set` (set or clear a goal) and the `GoalRound` notification (one per
judgement):

```
you>  /goal make the tests green
       → goal/set  {sessionId, condition: "make the tests green"}
       ← {active: true, condition: "make the tests green", roundsUsed: 0, maxRounds: 5}
… the turn ends without satisfying it …
       ← GoalRound {turnId, round: 1, maxRounds: 5, status: "continuing", reason: "two red"}
… another round …
       ← GoalRound {turnId, round: 2, maxRounds: 5, status: "satisfied", reason: "all green"}
```

`TurnMeta.goalStatus` / `goalRounds` carry the outcome and the count into the turn's
metadata.

`approval/respond`""",
        )
    ],
)

# internals 09
patch(
    "docs/internals/09-app-server.md",
    [
        (
            "`subagent.rs`（子代理执行体））。",
            "`subagent.rs`（子代理执行体）、`goal.rs`（目标循环））。",
        ),
        (
            "## ",
            """### 目标循环（`goal.rs`）

主循环停下来**不代表目标达成**：停下来只说明「这一步做完了」。设了目标的会话，
`run_turn` 会叫判断器（便宜模型、无工具、只看对话）看一遍，没达成就把理由当成下一条
用户消息**回到同一个会话**再跑一轮。

三处纪律都在代码里：轮次上限（`JOY_GOAL_MAX_ROUNDS`）到了就置 `round-limit` 并如实
说没完成；判断器调用失败就置 `blocked` 停续轮**并保住目标**；终止态记在目标上
（`Goal.status`），循环不再自己跑 —— 否则每一轮都会把同一个判断再问一遍。目标只有人
能设（`/goal` 或 `goal/set`），模型连这条路径都看不见。

`GoalRound` 通知是「为什么又跑了一轮」的唯一答案，REPL 每次都会打一行。

## """,
            1,
        ),
    ],
)

# CHANGELOG
patch(
    "CHANGELOG.md",
    [
        (
            "### Long commands can run in the background",
            """### Goals: keep going until the thing is actually done

`goal/set` (and `/goal` in the REPL) attaches a condition to a session. When the main
loop stops, a judge — cheap model, no tools, conversation only — decides whether the
goal is met; if not, its reason becomes the next user message and another round runs
in the same session. Every judgement emits `GoalRound` (round, max, status, reason)
so the extra rounds are explained, and `TurnMeta.goalStatus` / `goalRounds` carry the
outcome into the turn's metadata.

Bounded and honest: `JOY_GOAL_MAX_ROUNDS` (default 5) caps it, and hitting the cap
sets `round-limit` and says the goal was not met rather than pretending. A judge that
fails to answer sets `blocked`, stops the loop and **keeps** the goal. Terminal
outcomes are recorded on the goal so the loop does not re-ask the same question every
turn. Goals are set and cleared by humans only — the model has no tool for it, so
"the model assigning itself work" is not forbidden here, it is impossible.

### Long commands can run in the background""",
        )
    ],
)
patch(
    "CHANGELOG.zh.md",
    [
        (
            "### 长命令可以丢到后台",
            """### 目标：一直做到真的做完

`goal/set`（REPL 里是 `/goal`）把一个条件挂到会话上。主循环停下来时，判断器
（便宜模型、没有工具、只看对话）判一次目标是否达成；没达成，它的理由就变成下一条
用户消息，在同一个会话里再跑一轮。每次判断都发 `GoalRound`（轮次、上限、状态、理由），
于是多跑的轮次是有解释的；`TurnMeta.goalStatus` / `goalRounds` 把出口与轮次也带进
这一轮的元数据。

有界且如实：`JOY_GOAL_MAX_ROUNDS`（默认 5）封顶，到顶就置 `round-limit` 并说明
**没有达成**，而不是伪装完成。判断器答不出来就置 `blocked`、停续轮**并保住目标**。
终止态记在目标上，循环不会每一轮都把同一个问题再问一遍。目标只能由人设和清 ——
模型没有这个工具，所以「模型给自己派活」在这里不是被禁止，而是不存在。

### 长命令可以丢到后台""",
        )
    ],
)
print("goal 文档 ok")
