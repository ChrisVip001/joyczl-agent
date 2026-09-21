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
   ASCII 关键词重合度（≥2 个词命中）。中文不参与匹配——把英文触发词写进
   description 是格式要求，不是可选项。
2. **渐进披露。** frontmatter 每轮都扫（便宜）；正文只在命中时进
   system prompt；技能引用的文件只在模型开口要时才读。
3. **只描述做法，不假借工具。** 技能可以引用 Joy 的内置工具（如
   create_event），但 exported 到别的 agent 后那些工具不存在。

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

## 安装与分发

```bash
joy skill install <SKILL.md 的 url 或本地路径>   # 校验 + 重名拒绝
joy skill list                                   # 查看已装载
joy skill export --to claude,codex               # 复制给其他 agent（--project 落当前目录）
```

目录约定：`<home>/skills/<name>/SKILL.md`（`name` 必须是小写 slug）。
同名时 home 目录赢过 `JOY_SKILL_DIRS` 里的外部目录。技能正文里可以
引用同目录下的其他文件——渐进披露第三层：模型开口要才读。

## create_skill 工具

对话中教 Joy 一个可复用流程后，可以说"把这个流程存成技能"——它会调用
`create_skill` 写出合法的 SKILL.md。约定：**只在用户同意后调用**，
从不覆盖已有技能，名字强制小写 slug。
