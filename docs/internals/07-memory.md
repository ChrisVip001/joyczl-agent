# ⑦ 记忆层：`joyczl-memory`

「Joy 记得你」这件事由六个模块分工完成。它们共享同一条性格：**失败开放**——
门坏了照样查、摘要坏了退化摘录、向量坏了只用关键词。理由都一样：**过时的记忆
也好过丢失的记忆**。

目录：`joy-rs/joyczl-memory/src/`
（`gate.rs`、`consolidation.rs`、`compaction.rs`、`retrieval.rs`、`skills.rs`、
`install.rs`、`lib.rs`）。

| 模块 | 一句话 |
|---|---|
| `gate` | 这条消息要不要翻记忆？（hero moment #1：不相关的闲聊不查库） |
| `consolidation` | 攒够 N 轮，把对话提炼成 facts + 一条 episode |
| `compaction` | 滑窗外被挤掉的老轮次折成滚动摘要（不失忆） |
| `retrieval` | 关键词 + 向量两条腿，按名次融合 |
| `skills` | 过程记忆 SKILL.md：加载、触发、导出、安装 |
| `install` | 技能索引的安装/更新（取校验暂备原） |
| `lib.rs` | `retrieve_context`（拼提示词）与 `export_markdown`（MEMORY.md 镜像） |

## 7.1 `gate.rs`：检索门

**要解决的问题**：每轮都查记忆是浪费（一次查询换不来一句「2+2 等于几」的答案），
不查又会失忆。于是花一次**小模型**调用问一个二选一问题。

提示词（`GATE_PROMPT`）要求只回 JSON：

```json
{"retrieve": true, "query": "检索用的关键词", "reason": "五个词以内"}
```

并给了判据：通用知识/数学/闲聊/自足的请求 → false；涉及用户的生活、人、计划、
历史 → true。

**流程与容错**（`should_retrieve`）：

```
拼 GATE_PROMPT（把消息填进 {message}）
CreateRequest{ small_model, tools: [], max_tokens: 600 }    ← 600 给推理模型留思考量
client.create →
  Err          → Decision::fail_open("gate 调用失败，失败开放（{e}）", message)
  Ok(text)     → extract_json(text)
       None    → fail_open("gate 没返回 JSON — 失败开放", …)
       Some(j) → from_str
            Err → fail_open("gate 的 JSON 解析失败 — 失败开放", …)
            Ok(v)→ retrieve = v["retrieve"].as_bool().unwrap_or(true)   ← 缺失也当 true
                   query    = v["query"] 非空字符串，否则退回**原消息**
                   reason   = v["reason"] 或空
```

三个「失败开放」的具体形状值得注意：

- 失败时的 `query` 是**用户原话**，不是空串——门没给出检索词时用原话去查，
  比什么都不查更可能捞到东西。
- `extract_json` 只做「第一个 `{` 到最后一个 `}`」的截取，因为推理模型常在 JSON
  前后夹一段说明文字。
- `retrieve` 字段解析不出来时**默认 true**（宁可多查）。

`Decision` 还会原样出现在 `GateDecided` 通知与 `TurnMeta.gate` 里，所以界面上
能看到「门为什么放行/跳过」——可解释性顺手就有了。

## 7.2 `consolidation.rs`：从对话里提炼事实

**触发条件**：`chat.unconsolidated()` 拿到的行数 `< every_n * 2` 就返回 0
（一轮 = user + assistant 两行）。

**步骤**（`consolidate_if_due`）：

```
1. rows = unconsolidated()                    → Vec<(id, role, content)>
2. 行数不够 → Ok(0)
3. 拼 log（"role: content" 逐行）
4. 请求小模型：SUMMARIZER_PROMPT（要 facts 数组 + episode），max_tokens: 4096
       ← 4096 而不是 600：log 很长，600 会把推理模型的输出截成「只剩思考块」
5. client.create Err → Ok(0)                  （模型挂了就把这些行留着，下次再试）
6. extract_json / from_str 失败 → Ok(0)
7. facts：逐条取 subject/content，都非空才写；
       temporary_marker(content) 命中 → eprintln 跳过（见下）
       否则 facts.add(subject, content, "consolidation")
8. episode：非空则 episodes.add(今天, episode)
9. **全部成功之后**才 chat.mark_consolidated(&ids)
```

第 9 步的顺序是这一节的核心：**标记只在成功之后打**。顺序反了（先标记后提炼），
一次网络抖动就会永久跳过一段对话——原始日志还在，但没人再去提炼它。

### 临时陈述过滤（`TEMPORARY_MARKERS`）

19 个中英标记词：`本次会话` / `这次对话` / `当前对话` / `暂时` / `临时` / `先这样` /
`就这一次` / `仅限今天` / `this session` / `for now` / `temporarily` / `as a one-off` …
`temporary_marker` 大小写不敏感地子串匹配，命中就跳过并打一行 stderr。

理由：「这次会话先用方案 A」是关于**当下安排**的话，不是关于世界的事实；记进
记忆库等于往档案里塞草稿。这是一张词表而不是分类器——会漏掉不常见的表达，
但漏掉只是回到「多记一条」，不会误伤真事实（见 `docs/limitations.md`）。

### 写入去重（迁移 0008）

`facts` 上有一条唯一索引：`(subject, lower(trim(content)))`。`Facts::add` 走
`INSERT … ON CONFLICT DO NOTHING RETURNING`，没插进去就说明已经有了 —— 读回已存在
的那条，并返回 `(行, false)`。

* **为什么在写入口**：提炼每 N 轮跑一次、模型也常重复 `save_note`。没有去重时同一句
  话会一次次进库，检索结果里十条一模一样，而「这条有多可信」根本没法从重复次数上
  读出来 —— 重复不等于更多证据。
* **为什么返回 `bool`**：`written` 是「这一轮新增了多少」，把重复计进去会让账虚高；
  `save_note` 也要能说一句「这条已经记过了」而不是假装又记了一次。
* **为什么先清历史再建索引**：索引建不起来的话，之后每次写入都会报错 —— 迁移里先
  删掉已有的重复（保留 id 最小的那条），再立规。

### 类别（`kind`）与提炼退避

`facts.kind` 是「一条事实关于什么」：`user` / `feedback` / `project` /
`reference`，兜底 `fact`。**收敛在写入口**（`facts::normalise_kind`）：库里只会有
这五种，读的地方不必各自容错；模型写歪的分类落 `fact`，不丢。

提炼失败现在会**退避**（`chat::mark_consolidation_failed`）：`tries` 加一，
`next_at = now + min(60 × 2^tries, 3600)` 秒，`unconsolidated()` 只捞到点的行。
从前失败是「下次再来」—— 同一批坏行每轮都被重试一次，白烧模型调用，看起来还像
卡住了。成功仍然是 `consolidated = 1`，两套语义并行。

## 7.3 `compaction.rs`：上下文压缩

**要解决的问题**：滑窗（`JOY_HISTORY_TURNS`）是硬边界，更老的轮次不再进 prompt。
对长会话就等于失忆。

### 什么时候压：轮数是上限，token 才是闸门

`turns_that_fit(pairs, budget)` 从**最新**往回装，装到预算用完为止（至少留 1 轮）。
预算由 `app-server` 算：`窗口 × JOY_COMPACT_THRESHOLD` 减去「已经确定要花掉的部分」
（system 前缀、工具声明、这一轮的答案额度、摘要段的预留）。于是 `history_turns`
从「触发条件」降级为「上限」—— 一个长工具输出就能把窗口撑爆，而轮数看起来还很安全。

估算失败或估偏只会让压缩早/晚发生（见 ④ 的 `tokens.rs`），不会算错账。

### 水位线：`newly_evicted(pairs, window, covered)`

```
evicted_len = pairs.len().saturating_sub(window)     ← 理论上该被挤出的轮数
covered     = max(covered, 0)                        ← 摘要已经覆盖到哪
若 evicted_len <= covered → 返回空（没有新东西要折）
否则 → pairs[covered..evicted_len]                    ← 只取新增的那一段
```

`covered` 存在 `context_rollups.covered_turns`（②）。**只往前滚**：每次都拿
「上一版摘要 + 这次新挤出的轮次」交给模型，而不是重算全史。重算的成本随会话长度
线性增长，而摘要的意义正是别让成本这样长。

### 折叠：`roll_forward(...) -> (i32, String)`（永不返回 Err）

```
material = "Earlier summary:\n{上一版}\n\nNew turns to fold in:\n" + 每个 "User: …\nAssistant: …\n"
请求：system = SUMMARIZER_PROMPT，max_tokens: 700（摘要要短，给多了它会复述对话）
结果：
  Ok 且文本非空 → (covered_now, 摘要)
  Ok 但空串     → (covered_now, fallback_summary(…))
  Err           → (covered_now, fallback_summary(…))
```

`SUMMARIZER_PROMPT` 的要求是「保住每个事实、人名、决定、数字与未决线索；
丢掉客套与重复；用对话的语言写纯散文；不要标题、不要列表、不要评论任务本身」。

### 确定性兜底：`fallback_summary`

```
"（模型摘要不可用，以下是原对话的截断摘录：" + 上一版摘要 + " … "
  + 每轮 "用户：{user 截 120} ｜ 助手：{assistant 截 160} "
  + "）"
```

整体再截到 600 字符。**它不加工，只保证东西还在**——摘要再糙，也好过整段对话
凭空消失。这条路径有专门的 eval 用例钉住（⑫）。

### 注入：`summary_section`

```
None（空摘要）                      → 不加这一段
Some("\nEarlier in this conversation:\n{summary}")
```

调用方 `refresh()` 把「读水位线 → 算 → 存库」串起来，**存库失败只打 stderr**
（下次多花一次调用重算，不能连累这一轮对话）。

### 溢出兜底：压一次再试一次

估算再保守也可能撞上真实上限（各家分词器不同）。所以 provider 认得「上下文溢出」
（`ProviderError::ContextOverflow`：状态码 400/413/422 + 一份措辞短语表），
`full_turn` 接住它：**强制压缩**（保留最新 2 轮，其余折进摘要）后重试**一次**；
再溢出就如实报错。

两点克制：只有这一种错误值得重试（别的错误重试只会得到同样的错误，白等一倍时间）；
只有一次（第二次大概率还是同一个上限）。短语表认不出来时最多少一次重试 —— 错误
照常冒给用户，不会更糟。

### 两条刻意设计（写进 `docs/limitations.md`，别「修」）

1. **摘要不写回 `chat_log`**：`chat_log` 里永远是真对话，摘要住自己的表。
   「重看历史」因此看到的是原文——这是想要的行为。
2. **摘要不会被再摘要**：很长的会话就是一段越来越长的文字。分层滚动（摘要的
   摘要）没实现。

## 7.4 `retrieval.rs`：混合检索

**要解决的问题**：单靠关键词，「上次说的那个上线安排」找不到「10 月 15 日发版」
（字面没有一个词重合）；单靠向量，专名（`JOY_HOME`、`state.db`）又常常被糊掉。

### 相似度：`cosine(a, b) -> f32`

长度不等或是空 → 0.0（当作无关，不报错）；任一为零向量 → 0.0；否则标准余弦。

### 融合：`rrf_fuse(keyword, semantic) -> Vec<FactRow>`

```
对两路结果各自：score[id] += 1.0 / (RRF_K + rank + 1)      RRF_K = 60
    同名次并列时，先出现的（关键词那条腿）在前 —— 精确匹配更可信
排序：分数降序，同分按 id 升序（保证同一个查询每次得到同样的顺序）
输出：两路的**去重并集**
```

**为什么用名次而不是分数**：bm25 与余弦相似度量纲完全不同，硬凑成一个加权分数
是编数据；名次是两者都有的、可比的东西。k=60 是原论文的默认值，压住头部波动。

### 向量那一路：`semantic_hits`

```
embed(query) → 查询向量
all_with_embedding() → 每行 cosine ≥ MIN_SIMILARITY(0.30) 的留下
按分数降序、同分按 id 升序，取前 top_k
```

### 融合与降级：`search_hybrid(facts, embedder, query, top_k)`

| 情况 | 行为 |
|---|---|
| `embedder` 为 `None`（开关关着） | 直接返回关键词结果——**与升级前完全一样**（有测试钉这条） |
| `semantic_hits` 出错 | `eprintln` 一行「向量检索不可用，这次只用关键词」，返回关键词结果 |
| 向量结果为空 | 返回关键词结果 |
| 都有 | RRF 融合后取 `top_k` |

失败开放在这里的具体含义：**一条腿断了不致命**，绝不让一次网络故障变成
「什么都想不起来」。

### `reindex`：给旧事实补向量

```
循环：missing_embedding(50)（批大小 50）
      对每条：embed("{subject}: {content}") → set_embedding
      批空 → 返回补了多少条
      出错 → bail!("第 N 条算不出来（{e}）—— 已补 {done} 条")   ← 出错即中断并通报进度
```

`subject + content` 一起 embed：主题词常常正是检索时想问的词。

## 7.5 `skills.rs`：过程记忆（SKILL.md）

### 格式与解析

```markdown
---
name: weekly-review
description: 汇总这一周并起草周一简报
schedule: 0 8 * * 1        # 可选：带上就是一条定时任务（⑩）
version: 1.2.0             # 可选：`joy skill update` 靠它比版本
---

分步指令……
```

`parse_skill_text` 的规则：

- 必须以 `---\n` 开头，且存在 `\n---\n` 作为 frontmatter 的结束；
- 逐行 `split_once(':')`，值两侧去空白并去掉成对的单/双引号；
- **`name` 与 `description` 缺一就不算技能**；
- `schedule` / `version` 空值等于没写；
- 未知字段忽略（这样别人家的 frontmatter 扩展不会让技能读不出来）。

### 触发：关键词重合，透明可算

```
tokens(text)：小写 → 累积**连续 ASCII 字母数字**的 run → 长度 ≥3 才留下
触发：overlap = |tokens(name + description) ∩ tokens(message)|
      保留 overlap ≥ 2，按 overlap 降序取前 2 个
```

**中文不参与分词**（非 ASCII 字符一律打断 run）。所以技能描述里的触发词要写英文
——这是 Skills 格式的约定，也是 `docs/limitations.md` 里记着的一个真实边界。
好处是这个机制**完全透明**：为什么触发、为什么不触发，心算得出来；没有 embedding、
没有魔法。

### 渐进披露的三层（这一层的核心价值）

1. 每个技能的 frontmatter（name + description）**永远被扫**（便宜）；
2. 技能的**正文**只在触发时进 prompt（`matching_skills` 返回的 `### 名字\n正文`）；
3. 技能引用的其它文件只在模型开口要时才读（工具层的事）。

**热加载**：`SkillLoader` 记 `sig: Vec<(PathBuf, SystemTime)>`，每次
`match_message` 前比一次目录签名，变了就重扫。于是在会话中途 `create_skill`
写进去的技能，下一句就生效（有测试）。

### `dirs_for` 与优先级

```
dirs = [home/skills] + JOY_SKILL_DIRS（冒号分隔的额外目录，社区/自带技能包）
loaded_skills：home **最后**插入 → 同名覆盖额外目录里的（本地的赢）
跳过：SKILL.md 直接躺在技能根目录（不是 <名字>/SKILL.md）与 `_incoming/`（安装暂存）
```

### `is_slug`：唯一的名字规则

```rust
!name.is_empty()
  && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
  && !name.starts_with('-') && !name.ends_with('-')
```

这条规则**只有一个来源**，三条写入路径（`create_skill` 工具、`joy skill install`、
`joy skill update`）共用。教训来自一次真实的分叉：`install_from_text` 曾经放行
首尾带连字符的名字，另两条不放行——同一个名字在不同入口一边能过一边不能过。
现在有测试钉住三者一致。

### `install_from_text`（`joy skill install` 的执行体）

```
parse_skill_text 失败 → Err
!is_slug(name)       → Err（「它要变成目录名」）
目标已存在            → Err（**从不覆盖**——技能是指令，装之前先读一遍）
写原文 text 到 <home>/skills/{name}/SKILL.md
```

### `export_skills`（`joy skill export`）

目标目录由 CLI 算好（`~/.claude` / `~/.codex` / `--project` 时用当前目录），
落到 `<目标>/skills/<名字>/`。三条规矩：

- **内容相同 → unchanged**（`collect_files` 逐字节比）；
- **对方改过 → 默认保留**（"kept yours …"，`--force` 才覆盖）——那可能是人家在
  另一个 agent 里做的修改；
- 否则 `remove_dir_all` + `copy_dir`（递归复制，跳过 `.DS_Store` 与 `__pycache__`）。

### 两个策略字段与显式引用（`skills.rs`）

`allow-model-invocation: false` 的技能不参与隐式触发（`match_message` 里过滤），
依赖缺失的技能同样不参与（`missing_dependency`；启动时 `dangling_dependencies`
打一行日志）。两者之外的唯一入口是 `$技能名`。

`hits()` 是唯一入口，返回三样东西：拼进 system prompt 的段落、**剥掉 `$引用`
之后的消息**（引用是给 loader 看的，模型看正文就够）、以及引用了不存在的技能时
给模型的一句提示。剥掉的消息只影响**送给模型的那一份**；落库的历史仍是用户原话
（那是 `run_turn` 那边存的）。

一个易错点：`hits()` 里先只收**名字**再统一取正文 —— 先拿 `&Skill` 再调
`match_message(&mut self)` 会和借用检查器打架，而那不是「加个 clone」就完了的
问题：它提示的是「同一轮里技能的集合不应该变两次」。

## 7.6 `install.rs`：技能更新（取回→校验→比对→暂存→备份→原子替换）

顺序不是随便排的，每一步防一类事故：

| 步骤 | 防的是 |
|---|---|
| 先 `parse_skill_text` 校验 | 装进去一个解析不了的技能 = 把已有能力换成一个坏文件 |
| 先落 `.staging/{name}/` | 中途失败留下的是垃圾文件，不是坏掉的技能 |
| 替换前 `copy_dir` 到 `.backup/{name}-{时间戳}/` | 要回退就是一条 `mv` |
| `rename(staging, target)` | 同文件系统内原子替换 |
| 名不对版（索引说 A、文件写 B）拒绝 | 索引与目录从此对不上 |

**版本比较**（`needs_update`）：`installed` 为 `None` → true；串相等 → false；
两边都能解析成点分数字 → `offered > installed`；**比不了大小就不动**
（「不降级」优先于「尽量更新」）。

**结果分类**（`Outcome`）：`Installed{from,to}` / `UpToDate` / `Ahead`
（本地更新，没动）/ `Failed{why}`，各有一句人读的 `line()`。

**取回是注入的**（`update_all<F, Fut>(home, entries, fetch)`）：这一层**不认识网络**，
所以它的测试不需要网络。CLI 那边接的是 HTTP（走 `raw_url` 把 GitHub 页面地址转成
raw 地址）或本地文件读取。

## 7.7 `lib.rs`：两个对外函数

**`retrieve_context(facts, episodes, query, top_k, embedder) -> String`**

```
事实线：retrieval::search_hybrid(...)      ← 向量开关关着时就是纯关键词
情景线：episodes.search(query, 3)          ← 情景带日期，向量帮不上多少
拼：facts → "- **subject**: content"；episodes → "- (happened_at) summary"
空字符串 = 这次没检索到 → 调用方不往 prompt 里塞「相关记忆」标题
```

**`export_markdown(facts, episodes, home)`**：生成 `<home>/MEMORY.md`——
`# Joy memory` + 一段斜体说明（**事实来源是 state.db，本文件每轮重新生成**）+
「## Facts —— 语义记忆（N）」+「## Episodes —— 情景记忆（N）」。

它是**生成的视图，不是事实来源**。这句话必须写在文件里：否则有人会去编辑它，
然后发现改动被下一轮覆盖。「你的记忆就是一个能打开的文件」这句话要成立，
但得说清是只读的一张照片。
