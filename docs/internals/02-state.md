# ② 存储层：`joyczl-state`

全部持久状态都在**一个 SQLite 文件**里。这一层管的是「怎么开它、表长什么样、
每条查询的语义」，不管「什么时候写」（那是 ⑨ 服务端与 ⑦ 记忆层的事）。

目录：`joy-rs/joyczl-state/`（`src/` + `migrations/`）。

## 2.1 `open()`：七步，每一步都有理由

`src/db.rs（open）`：

1. **建父目录**：`create_dir_all(parent)`，失败带上下文报错。
2. **绝对路径化**：`sqlite://` 后面必须是绝对路径。相对路径会被 sqlx 解释成
   「相对于进程 cwd」，而 **app-server 的 cwd 不该决定数据库在哪** —— 在别处启动
   一次就会静默建出一个新的空库。
3. **按需建库**：`Sqlite::database_exists` → 不存在则 `create_database`。
4. **连接池**：`SqlitePoolOptions::max_connections(8)`。
5. **`PRAGMA busy_timeout = 3000`**：3 秒等锁。单进程内多个请求并发写时，不会
   直接甩一个 "database is locked"。
6. **`PRAGMA journal_mode = WAL`**：读不阻塞写。驾驶舱一边轮询一边聊天不会被卡。
7. **跑迁移**：`MIGRATOR.run(&pool)`。

**迁移是编译期内嵌的**（`static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations")`）：
`sqlx` 的 macros feature 在编译期把 `.sql` 读进二进制。所以发布出去的单个二进制
自带 schema，**不依赖运行时找文件**——这正是「一个二进制、一个状态目录」能成立的
前提。

多进程共存是**刻意支持**的（WAL + busy_timeout 就是为此）：app-server、REPL、
`joy mcp serve`、`joy schedule` 各开各的连接。代价写进了 `documentation`：
**没有东西协调跨进程的「轮次」**，两个进程里的两轮对话会自由交错。

## 2.2 五个迁移，逐个讲

### `0001_init.sql` — 基础 schema

三张表 + 两个 FTS 影子表 + 触发器：

```sql
facts(id, subject, content, source DEFAULT 'user', created_at)
facts_fts USING fts5(subject, content, content='facts', content_rowid='id')   -- external content
episodes(id, happened_at, summary, created_at)
episodes_fts USING fts5(summary, content='episodes', content_rowid='id')
chat_log(id, role, content, consolidated DEFAULT 0, session_id DEFAULT 'default',
         source DEFAULT 'cli', meta, created_at)
+ index chat_log(session_id)、index chat_log(consolidated)
```

**external content 表**的意思是：FTS 表不存正文副本，只存索引，正文仍从 `facts`
读（`content='facts'` 指明来源表，`content_rowid='id'` 指明 rowid 对齐哪一列）。
代价是它**不会自动同步**，所以需要三个触发器：

- `facts_ai`（AFTER INSERT）：往 FTS 插一行；
- `facts_ad`（AFTER DELETE）：用 FTS5 的 `'delete'` 指令删索引行；
- `facts_au`（AFTER UPDATE）：先删旧值再插新值。

`episodes` 只有 ai/ad 两个（它不更新）。

### `0002_cjk_trigram.sql` — 换分词器（重要的一课）

**问题**：FTS5 默认的 `unicode61` 把一整串连续中文当成**一个词**。于是
「阿明喜欢早上的会议」查「早上」匹配不到——中文没有空格，默认分词器对中文基本
等于不工作。

**做法**：drop 掉旧的 FTS 表与三个触发器，用 `tokenize='trigram'` 重建，再
`INSERT INTO facts_fts(facts_fts) VALUES ('rebuild')` 重建索引（换了分词器，
旧索引格式已经不对，这一步不能省）。episodes 同理。

**trigram 是什么**：按连续三字符切片建索引。「早上的会议」会切成
`早上上`、`上上的`、`上的会`…这样的三元组。于是任意 ≥3 字符的查询都能命中；
英文顺带拿到子串匹配（`demo` 命中 `demos`）。

**代价**（注释里写了）：索引约 3 倍大；**查询词短于 3 字符切不出三元组**。
后者就是 `fts.rs` 里那条 LIKE 回退存在的理由（见 2.4）。

### `0003_calendar.sql` — 本地日历

```sql
calendar_events(id, title, start, "end", attendees DEFAULT '', notes DEFAULT '', created_at)
CREATE UNIQUE INDEX calendar_events_title_start_idx ON calendar_events (title, start);
CREATE INDEX calendar_events_start_idx ON calendar_events (start);
```

`end` 是 SQL 保留字，必须加引号（Rust 侧的查询也照写 `"end"`）。
那个 **UNIQUE(title, start)** 就是「同一场会议不会被预订三次」的全部实现——
幂等交给 SQL，代码里不写「先查再插」那种有竞态的检查。

### `0004_context_rollups.sql` — 滚动摘要

```sql
context_rollups(session_id PRIMARY KEY, covered_turns, summary, updated_at)
```

`covered_turns` 是**水位线**：这段摘要已经覆盖了最早的多少轮。只往前滚，
每次把滑窗新挤出的几轮折进现有摘要，而不是重算全史（重算的成本随会话长度线性
增长，而摘要的意义正是别让成本这样长）。见 ⑦ 的 compaction 章。

### `0005_embeddings.sql` — 向量列

```sql
ALTER TABLE facts ADD COLUMN embedding TEXT;   -- JSON 数组文本，NULL = 还没算过
```

**为什么是一列文本而不是向量扩展**（比如 sqlite-vec）：个人记忆的规模是几千条，
全表扫描 + Rust 侧算余弦在毫秒级；而引入向量扩展会把这棵树的构建依赖换成平台
相关的二进制。规模真到了那天，**这一列正好是迁移的起点**。
没有向量的行照样参与关键词检索，所以打开开关之前的记忆不会消失。

## 2.3 每张表的 API（语义都写在签名里）

### `Facts`（语义记忆）

| 方法 | SQL | 语义要点 |
|---|---|---|
| `add(subject, content, source)` | `INSERT … RETURNING id, …, created_at` | 返回完整行，调用方不必再查一次拿 id |
| `search(query, top_k)` | 见 2.4 | 两级：FTS5 → LIKE 回退 |
| `recent(limit, offset)` | `ORDER BY id DESC LIMIT ? OFFSET ?` | |
| `all_by_subject(limit)` | `ORDER BY subject, id LIMIT ?` | 给 `MEMORY.md` 镜像用（同一主题的事实待在一起才像记忆） |
| `forget_subject(subject)` | `DELETE … WHERE subject = ?` | 返回 `rows_affected`；触发器同步清 FTS |
| `update(id, content)` / `delete(id)` | `UPDATE`/`DELETE … WHERE id = ?` | 返回 `bool`：**id 不存在返回 false 而不是报错** |
| `set_embedding(id, &[f32])` / `all_with_embedding()` / `missing_embedding(limit)` | | 向量是加分项，解析不了的行跳过 |

### `Episodes`（情景记忆）

`add(happened_at, summary)`（`RETURNING id`）、`search`（同两级）、`recent(limit)`、
`delete(id) -> bool`。

### `Chat`（对话日志）

| 方法 | 说明 |
|---|---|
| `append_exchange(user, assistant, session_id, source, meta)` | **两条** INSERT：user 行不带 meta，assistant 行带 meta（遥测只属于回答那一侧） |
| `unconsolidated()` | `WHERE consolidated = 0 ORDER BY id`，返回 `(id, role, content)` |
| `mark_consolidated(&[i64])` | 空 slice 直接 Ok；否则 `IN (?,?,…)` 拼占位符。**只在提炼成功之后调用**（见 ⑦） |
| `session_history(session_id)` | 按 id 升序取所有行，Rust 侧把 user/assistant 配成「轮次」对（user 进 pending，assistant 取出配对） |
| `messages(session_id, before, limit)` | 给驾驶舱翻页用，`before` 默认 `i64::MAX`（一句 SQL 而不是两句） |
| `sessions()` | `GROUP BY session_id` + 相关子查询取首条 user 消息当标题，空会话兜底 `"(空会话)"` |
| `load_rollup(session_id)` / `save_rollup(…)` | 滚动摘要的读写，`ON CONFLICT DO UPDATE`（一会话一行） |

### `Calendar`

| 方法 | 说明 |
|---|---|
| `add(title, start, end, attendees, notes)` | `INSERT OR IGNORE … RETURNING …`；冲突时返回 `Ok(None)` —— **幂等靠它，不靠调用方检查** |
| `list(start, end, limit)` | `QueryBuilder` 动态拼 WHERE，用 `substr(start, 1, 10) >= ?` 只比日期部分（ISO 前缀相等即同一天） |

**一条贯穿全层的契约**（`src/store.rs` 头部与 `conformance.rs`）：所有方法返回
`anyhow::Result`，但 **miss 绝不报错** —— 查不到就是空 `Vec`，删不到就是 `false`，
忘不掉就是 `0`。理由：调用方是模型，它需要一句「没有」而不是一个异常。

`SemanticStore` / `EpisodicStore` 两个 trait 是给「将来换后端」留的缝，
`conformance.rs` 里的 `exercise_semantic` / `exercise_episodic` 是**任何后端都必须
通过的契约测试**（写入→命中、update 不存在返回 false 不报错、delete 两次第二次
false、乱码查询返回空不报错……）。本家 SQLite 实现也必须跑。

## 2.4 FTS 查询的构造：两个纯函数

### `fts.rs（to_match_expr）`

把用户查询变成合法的 FTS5 表达式：

1. 按**非字母数字**切分（`split(|c: char| !c.is_alphanumeric())`）。不是只削首尾——
   `a:b` 会让 FTS5 语法报错，内部标点必须切掉。
2. 每个词加双引号：`alex` → `"alex"`。加引号后语法**永远合法**。
3. 用 ` OR ` 连接：`"alex" OR "morning"`。语义是「命中任意词即可」。

中文因为 `char::is_alphanumeric()` 为真而**不被切碎**，整块当引号短语交给 trigram
分词器按三字符切片索引——这正是换分词器后中文能用的原因。

### `fts.rs（like_pattern）`

LIKE 回退的模式串：

1. **过滤掉 `%` 与 `_`** —— 否则用户可以把 LIKE 变成通配全表扫描。
2. `trim()`，若不含任何字母数字返回 `None`（全是标点的查询不查）。
3. 否则 `%{needle}%`。

### 为什么两级（`facts.rs` / `episodes.rs` 里的顺序）

```
先 FTS5（trigram）→ 命中就返回
否则 LIKE 子串扫描
两个都是 None（全是标点）→ 返回空 Vec，不报错
```

typo 之外的真正理由：trigram 对**短于 3 字符**的查询词无解，而**中文两字词非常
常见**（「早上」「阿明」）。此时 FTS 必然 miss，LIKE 兜底；个人库几千行，
一次全表 LIKE 是微秒级，不值得为它引外部分词器。

## 2.5 测试与不变量

- `state_tests.rs`：表 API 的行为（含「删两次第二次 false」这类契约）。
- `conformance_tests.rs`：契约测试 + 一条**回归测试**
  （`trait_objects_and_inherent_calls_agree`）：trait 方法与固有方法并存时结果必须
  一致——库里出现过「委托实现和原实现行为不一致」的坑。
- 测试里建临时库后 `let _ = dir.keep();`（而不是让 `TempDir` 在 drop 时删目录），
  因为 sqlite 还要写 `-wal`/`-shm`，目录提前消失会让后续写入报错。

**这一层的不变量**：schema 只由迁移产生；任何表都只能通过上面那些方法访问；
miss 不报错；多进程共存是支持的，跨进程的轮次协调不是。
