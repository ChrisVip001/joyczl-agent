# Skill authoring guide

English | [简体中文](skills.zh.md)

A skill is Joy's procedural memory: one `SKILL.md` describing **how** to do a
class of task, loaded automatically when a relevant message appears. Official
Agent Skills format:

```markdown
---
name: weekly-review
description: Summarize the week and draft the Monday brief
---

1. Pull last week's episodes from memory.
2. Distill into three bullets.
3. Draft the digest into the outbox.
```

## Three rules

1. **The `description` doubles as the trigger.** Triggering = keyword overlap
   between the message and name+description (≥2 ASCII word hits). Chinese
   does not participate — writing English trigger words into the description
   is a format requirement, not an option.
2. **Progressive disclosure.** The frontmatter is scanned every turn (cheap);
   the body enters the system prompt only on a match; files a skill references
   are read only when the model asks.
3. **Describe the process, never borrow tools.** A skill may reference Joy's
   built-in tools (e.g. create_event), but once exported to another agent
   those tools do not exist.

## Install and distribute

```bash
joy skill install <SKILL.md url or local path>   # validated + collision refused
joy skill list                                   # what is loaded
joy skill export --to claude,codex               # copy to other agents (--project targets the cwd)
```

Directory convention: `<home>/skills/<name>/SKILL.md` (`name` must be a
lowercase slug). On name collision the home directory wins over
`JOY_SKILL_DIRS`. A skill may reference sibling files — disclosure tier
three: read only when the model asks.

## The create_skill tool

After teaching Joy a reusable flow in conversation, say "save this as a
skill" — it calls `create_skill` to write a valid SKILL.md. The rules: **only
after the user agrees**, never overwrite an existing skill, lowercase slug
names enforced.
