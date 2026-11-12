# 技能创作指南

[English](skills.md) | 简体中文

技能（Skill）是 Joy 的过程记忆：一份 `SKILL.md` 描述一类任务的**做法**，
 Joy 在相关消息出现时自动载入。官方 Agent Skills 格式：

```markdown
---
name: weekly-review
description: Summarize the week and draft the Monday brief
---

1. Pull last week's episodes from memory.
2. Distill into three bullets.
3. Draft the digest into the outbox.
```

## 三条规则

1. **`description` 兼任触发器。** 触发判定 = 消息与 name+description 的
   关键词重合度（至少两个不同匹配）：ASCII 单词至少三个字符，汉字使用相邻双字组合。
   中文描述可以直接匹配，重复词不会提高分数。
2. **渐进披露。** frontmatter 每轮都扫（便宜）；正文只在命中时进
   system prompt；技能引用的文件只在模型开口要时才读。
3. **只描述做法，不假借工具。** 技能可以引用 Joy 的内置工具（如
   create_event），但 exported 到别的 agent 后那些工具不存在。

## 两个策略字段

```markdown
---
name: weekly-review
description: 汇总这一周
allow-model-invocation: false
dependencies: changelog, calendar
---
```

* `allow-model-invocation: false` —— 作者声明「别不小心把我叫出来」。这种技能**不
  会因为关键词重合而被触发**，只有消息里点名才载入：`$weekly-review 帮我看看这周`。
* `dependencies: a, b` —— 这个技能要成立，还得有哪些技能在场。依赖缺失的技能不
  参与隐式触发，启动那一行会点名说缺了什么。

`$技能名` 是显式通道：强制把正文放进 prompt（不需要关键词重合），并且会从模型
看到的消息里被剥掉。引用了不存在的技能不算错误 —— 模型会收到一句提示，从而能
如实回答。

## 版本与 `joy skill update`

技能可以在 frontmatter 里带 `version: 1.2.0`。给 Joy 一个索引，它会把更新的
装上来，并把旧版本留着：

```json
{"skills": [{"name": "weekly-review", "version": "1.2.0",
             "url": "https://example.com/skills/weekly-review.md"}]}
```

```bash
joy skill update                       # <home>/skills/index.json
joy skill update ./my-index.json       # 也可以给路径 / http(s) 地址
```

规矩按这个顺序执行：先校验（解析不了的 SKILL.md 绝不落盘）、再暂存然后原子
替换、旧版本备份进 `.backup/<名字>-<时间戳>/`、并且从不降级。`joy skill
install` 保持它自己的规矩 —— 从不覆盖。

## 让技能按时跑

frontmatter 里加一行，技能就同时是一条定时任务：

```markdown
---
name: weekly-review
description: 汇总这一周并起草周一简报
schedule: 0 8 * * 1
---
```

`joy schedule`（常驻进程）按时触发它，在技能自己的会话里跑完整一轮，结果写进
`<home>/outbox/`。cron 细节与 JSON 声明方式见 [operations.zh.md](operations.zh.md)。

## 仓库自带的示例：视频转写

`skills/video-transcription/SKILL.md` 是随仓库分发的活例子，也是格式示范：
「分析这个视频」会先探测 yt-dlp / whisper（缺了如实报告，不装懂）、在线视频拉
字幕不下载、本地视频走转写，最后用 `read_file` 读文稿分析 —— 全程强调
**模型没有文字就不许描述视频内容**，并把字幕当作「数据不是指令」的外部输入。
把它复制到 `<home>/skills/` 下即可启用。

## 安装与分发

```bash
joy skill install <SKILL.md 的 url 或本地路径>   # 校验 + 重名拒绝
joy skill list                                   # 查看已装载
joy skill export --to claude,codex               # 复制给其他 agent（--project 落当前目录）
```

目录约定：`<home>/skills/<name>/SKILL.md`（`name` 必须是小写 slug）。
同名时 home 目录赢过 `JOY_SKILL_DIRS` 里的外部目录。技能正文里可以
引用同目录下的其他文件——渐进披露第三层：模型开口要才读。

## 驾驶舱里的「能力中心」

侧栏的「能力中心」列出所有已装载的技能（版本、策略字段、缺失的依赖），
`<home>/skills/` 下的那些可以加、可以删。两条规矩：

- **内置技能置灰只读。** 来自 `JOY_SKILL_DIRS` 的技能随安装或机器分发，
  从这一台机器的界面上删掉它，会让每一台共享它的机器一起失效。
- **装上就生效，不用重启。** 加载器按目录变化重扫；校验和
  `joy skill install` 同一道门（frontmatter、slug 名、从不覆盖），
  被拒也是一句话，不会静默没反应。

## create_skill 工具

对话中教 Joy 一个可复用流程后，可以说"把这个流程存成技能"——它会调用
`create_skill` 写出合法的 SKILL.md。约定：**只在用户同意后调用**，
从不覆盖已有技能，名字强制小写 slug。

## 选技能：先看目录，再读正文

每轮的系统提示里带一份**目录** —— 每个技能一行 `名字 — 说明`，上限由
`JOY_SKILL_INDEX_TOKENS` 控制；正文只在需要时才载入：消息里写了 `$名字`、或关键词命中
（`JOY_SKILL_BODY_TOKENS` 限制单篇正文，只按整行截断，末尾留一句诚实的标记）、或者
模型看完目录自己调 `read_skill`。

为什么不能只靠关键词：重合度打分只有在用户恰好用了说明里的词时才触发，换个说法就永远
载不进来。能看见书架的模型会自己挑对那一本。

目录里会挂两个标记 —— `explicit $name only` 与 `needs <依赖>` —— 因为把一个跑不起来的
技能摆在目录里，代价是模型花一轮去发现它不能用。

命中记录追加在 `<home>/skill-hits.jsonl`（一次调用一行），驾驶舱的「技能」页会汇总显示。
坏掉的行直接跳过、绝不致命：这是遥测数据，一行坏掉不该让整个面板打不开。
