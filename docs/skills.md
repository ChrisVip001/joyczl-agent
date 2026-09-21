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

## Two policy fields

```markdown
---
name: weekly-review
description: summarize the week
allow-model-invocation: false
dependencies: changelog, calendar
---
```

* `allow-model-invocation: false` — the author says "do not summon me by
  accident". Such a skill never fires from keyword overlap; it loads when the
  message names it: `$weekly-review help me with this week`.
* `dependencies: a, b` — skills that must be present for this one to make
  sense. A skill with a missing dependency does not fire implicitly, and the
  startup line names what is missing.

`$name` is the explicit channel: it forces the body into the prompt (no keyword
overlap needed) and is stripped from the message the model sees. A reference to
a skill that does not exist is not an error — the model gets a line saying so,
and can answer honestly.

## Versioning and `joy skill update`

A skill can carry `version: 1.2.0` in its frontmatter. Point Joy at an index
and it installs whatever is newer, with the previous copy kept:

```json
{"skills": [{"name": "weekly-review", "version": "1.2.0",
             "url": "https://example.com/skills/weekly-review.md"}]}
```

```bash
joy skill update                       # <home>/skills/index.json
joy skill update ./my-index.json       # or a path / http(s) URL
```

Rules, in order: validate first (a SKILL.md that will not parse is never
written), stage then swap atomically, back up the old version to
`.backup/<name>-<timestamp>/`, and never downgrade. `joy skill install` keeps
its own rule — it refuses to overwrite anything.

## Letting a skill run on a schedule

Adding one frontmatter line turns a skill into a scheduled job:

```markdown
---
name: weekly-review
description: summarize the week and draft the Monday brief
schedule: 0 8 * * 1
---
```

`joy schedule` (a resident process) fires it, running a full turn in the
skill's own session and writing the answer to `<home>/outbox/`. See
[operations.md](operations.md) for the cron details and the JSON alternative.

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
