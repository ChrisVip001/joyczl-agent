import io


def patch(path, pairs):
    d = io.open(path, encoding="utf-8").read()
    for old, new in pairs:
        assert old in d, f"{path}: 缺锚点 {old[:70]}"
        d = d.replace(old, new, 1)
    io.open(path, "w", encoding="utf-8").write(d)


# ---- limitations：并发一节（中英）----
patch(
    "docs/limitations.zh.md",
    [
        (
            "## 记忆",
            """## 并发

* **刻意** —— 同一会话的两个并发 turn **没有串行保护**（`turn/start` 是并发
  处理的，这是为了 `turn/interrupt` 能进来）。后果都收在下面几条里，没有一条
  会损坏数据：提炼是**原子认领**（输的那个不调模型）、滚动摘要有**单调守卫**
  （只接受覆盖更多轮次的那份）、记忆写入有**去重闸**。剩下的只是「多等一会儿」。
  → `joy-rs/joyczl-memory/src/consolidation.rs`、`joyczl-state/src/chat.rs`
* **刻意** —— SQLite 的等锁上限是 **10 秒**（挂在连接选项上，每个连接都带上）。
  多进程同时写（REPL、dashboard、`joy mcp serve`、`joy schedule`）时排队而非
  直接失败；10 秒还不够就只能减小并发面，而不是把超时调得更长。
  → `joy-rs/joyczl-state/src/db.rs`
* **未做** —— 没有 per-session 锁。要加的话是在 turn 的**写入段**（append /
  提炼 / 压缩）持锁、模型调用段不持；收益是「同会话严格有序」，代价是第二个
  turn 会明显变慢。当前不需要：上面几条已经把并发后果降到可接受。
* **未做** —— `prune_spill` 的启动清理与另一进程的写入有理论竞争（只删 7 天前
  的文件，实际概率极低）。

## 记忆""",
        )
    ],
)
patch(
    "docs/limitations.md",
    [
        (
            "## Memory",
            """## Concurrency

* **Deliberate** — Two concurrent turns on the **same session are not serialised**
  (`turn/start` is handled concurrently so `turn/interrupt` can get in). The
  consequences are contained by three guards, none of which can corrupt data:
  consolidation is an **atomic claim** (the loser never calls the model), the
  rolling summary has a **monotonic guard** (only a summary covering more turns
  is accepted), and memory writes have a **de-duplication gate**. What is left is
  "waits a bit longer".
  → `joy-rs/joyczl-memory/src/consolidation.rs`, `joyczl-state/src/chat.rs`
* **Deliberate** — SQLite waits **10 seconds** for a lock, set on the connect
  options so every connection carries it. Multiple processes writing at once
  (REPL, dashboard, `joy mcp serve`, `joy schedule`) queue instead of failing;
  if 10s is not enough the answer is fewer concurrent writers, not a longer
  timeout.
  → `joy-rs/joyczl-state/src/db.rs`
* **Not done** — No per-session lock. Adding one means holding it during the
  turn's *write* section (append / consolidate / compact) but not during model
  calls; the gain is strict per-session ordering, the cost is a visibly slower
  second turn. Not needed today: the three guards above already make the
  concurrency consequences acceptable.
* **Not done** — `prune_spill` at startup has a theoretical race with another
  process writing (it only deletes files older than seven days).

## Memory""",
        )
    ],
)

# ---- internals 07-state：并发的闸门 ----
patch(
    "docs/internals/07-state.md",
    [
        (
            "## ",
            """## 并发的三个闸门

`state.db` 是多进程共享的（REPL / dashboard / `joy mcp serve` / `joy schedule`
可以同时开着它），进程内又有并发的 turn。三处 read-then-act 各有各的闸门：

| 位置 | 闸门 | 挡住什么 |
|---|---|---|
| `consolidate_if_due` | `claim_unconsolidated`：一条 `UPDATE … RETURNING` | 两个并发 turn 同时看到「到期」→ 双份提炼（白烧一次模型调用） |
| `save_rollup` | `WHERE excluded.covered_turns > context_rollups.covered_turns` | 并发压缩时旧摘要盖掉新摘要 |
| `Facts::add` | 先查后写 + 唯一索引 | 重复提炼 / 重复 `save_note` 堆出重复事实 |

连接参数（`busy_timeout` 10s、WAL）必须挂在 `SqliteConnectOptions` 上：
`PRAGMA busy_timeout` 是 per-connection 的，`execute(&pool)` 只设中当时那个
连接 —— 这条坑有测试钉着（`the_pool_waits_for_locks_and_uses_wal`）。

## """,
            1,
        ),
    ],
)

# ---- CHANGELOG ----
ZH = """### 并发：两个 turn 撞上的三处闸门

同一会话的两个并发 turn 面前有三处 read-then-act：提炼是「读一批 → 蒸馏 → 再
标记」，于是两边都看到「到期」，同一批日志被蒸馏两遍（白烧一次模型调用）；滚动
摘要的 `ON CONFLICT … DO UPDATE` 不看谁的 `covered_turns` 更新，并发压缩时用户
可能拿到一份**更旧**的摘要；`busy_timeout` 写在注释里是 3 秒、实际是 sqlx 的默认
5 秒（`PRAGMA` 是 per-connection 的，`execute(&pool)` 只设中当时那个连接）。

三处各自收口：提炼改成一条 `UPDATE … RETURNING` 的**原子认领**，输的那个拿到空
列表、**连模型都不调**（失败路径把认领还回去，`consolidated` 归零 + 指数退避）；
滚动摘要加**单调守卫**，只接受覆盖更多轮次的那份，被挡下时 stderr 说一句；
等锁改成挂在连接选项上的 **10 秒**（SQLite 官方建议值）。另外把后台作业的 outbox
文件名加上 pid —— job id 是进程内自增的，两个 Joy 进程会写出同名 `j1.txt`。

没有加 per-session 锁：代价（第二个 turn 明显变慢）大于收益，三条闸门已经把并发
后果降到「多等一会儿」。测试：并发两次提炼断言小模型只被调用一次、失败释放认领、
摘要只往前滚、PRAGMA 真的生效（这条测试当场抓出「注释说 3 秒、实际 5 秒」）。

"""
EN = """### Concurrency: three guards where two turns meet

Two concurrent turns on the same session used to meet three read-then-act races:
consolidation read a batch, distilled it, then marked it, so both sides saw "due"
and distilled the same log twice, burning a model call; the rolling summary's
`ON CONFLICT … DO UPDATE` ignored which `covered_turns` was newer, so concurrent
compaction could hand the user an **older** summary; and `busy_timeout` read 3
seconds in the comment while the real value was sqlx's default 5 (`PRAGMA` is
per-connection and `execute(&pool)` only configures whichever connection served
that query).

Each is now closed at its own door: consolidation became a single
`UPDATE … RETURNING` **atomic claim**, so the loser gets an empty list and
**never calls the model** (failure paths release the claim — `consolidated` back
to 0 plus exponential backoff); the rolling summary gained a **monotonic guard**
that only accepts a summary covering more turns, and says so on stderr when it
refuses; and the lock wait moved onto the connect options at **10 seconds**,
SQLite's recommended value. Background job outbox filenames also carry the pid —
job ids are per-process, so two Joy processes used to write the same `j1.txt`.

No per-session lock: the cost (a visibly slower second turn) outweighs the gain
now that the three guards reduce the concurrency consequences to "waits a bit
longer". Tests: two concurrent consolidations assert the small model is called
once, a failed consolidation releases its claim, a rollup only moves forward, and
the PRAGMAs really are in effect — that last test caught "3 seconds in the
comment, 5 seconds in reality" on the spot.

"""
for path, anchor, block in [
    ("CHANGELOG.md", "### Goals: keep going until the thing is actually done", EN),
    ("CHANGELOG.zh.md", "### 目标：一直做到真的做完", ZH),
]:
    d = io.open(path, encoding="utf-8").read()
    assert anchor in d, f"{path}: {anchor[:40]}"
    d = d.replace(anchor, block + anchor, 1)
    io.open(path, "w", encoding="utf-8").write(d)
print("并发文档 ok")
