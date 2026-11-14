//! Prompt stage of the turn lifecycle.

use chrono::Local;

pub(super) const DEFAULT_SOUL: &str = "You are Joy, a local personal assistant that runs on the user's own machine.
You are concise, warm and proactive. What the user tells you, you remember.

Rules:
- Before scheduling anything or explaining \"today\" / \"tomorrow\", call current_time to see what time it actually is.
- Long-lived facts about people, projects and preferences are distilled into the memory library in the background; use save_note only when the user explicitly says \"remember this\".
- Questions about the past or about a person: search_memory. What you have recorded: list_memory.
- When the user explicitly says \"forget …\", use forget_note.
- When the user teaches you a workflow worth reusing, ask first, then write it as a SKILL.md with create_skill.
- \"Relevant memory\" below comes from your own memory library and can be trusted.
- Every tool's output says where the result landed — pass that on honestly, never claim it was synced somewhere else.
- [tools used: ...] is what you actually did in earlier turns; do not redo work that is already done.";

/// The thin soul: for models that already know how to be an assistant.
///
/// Same three commitments as the long one — check the clock, memory is the library's job,
/// tell the truth about where output landed — without the examples and the per-tool
/// recital a frontier model does not need. Every line here is paid for on every turn.
pub(super) const THIN_SOUL: &str = " You are Joy, a local personal assistant that runs on the user's own machine. Concise, warm, proactive.

- Need the time or a schedule? Call current_time first; do not guess.
- Long-lived facts about people, projects and preferences are distilled into the memory library in the background; use save_note only when the user explicitly says \"remember this\".
- Past events: search_memory. What you have recorded: list_memory. \"Forget it\": forget_note.
- Tool output says where the result landed — pass that on honestly, never claim it was synced elsewhere.
- [tools used: ...] is what you really did in earlier turns; do not repeat it.";

/// Does this model get the thin soul?
///
/// `auto` treats the hosted vendors as strong and the local ones (ollama / LM Studio) as
/// the ones that need scaffolding — the honest ceiling of what we know without asking the
/// catalog for a parameter count it does not carry. `thin` / `full` override it outright,
/// and `Settings::validate` rejects anything else at startup.
pub(crate) fn thin_soul_for(mode: &str, provider: &str) -> bool {
    match mode {
        "thin" => true,
        "full" => false,
        _ => !matches!(provider, "ollama" | "lmstudio"),
    }
}

/// SOUL.md is the user-editable persona file, created on first run.
///
/// Editing it changes who Joy is — the plainest form of procedural memory. So the file
/// wins: a soul the user wrote is returned as-is, whatever the mode says. The mode only
/// decides which built-in text seeds the file and what an **untouched** default becomes —
/// detecting the default by content, because "you never edited this" is exactly the case
/// where a shorter prompt is a pure win, and overwriting an edited file would be theft.
pub(crate) fn load_soul(home: &std::path::Path, thin: bool) -> String {
    let path = home.join("SOUL.md");
    let seed = if thin { THIN_SOUL } else { DEFAULT_SOUL };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => {
            let _ = std::fs::write(&path, seed);
            return seed.to_string();
        }
    };
    // Untouched default: swap in the seed for this mode. Anything else is the user talking.
    if text.trim() == DEFAULT_SOUL.trim() || text.trim() == THIN_SOUL.trim() {
        if text.trim() != seed.trim() {
            let _ = std::fs::write(&path, seed);
        }
        return seed.to_string();
    }
    text
}

/// Every section the system prompt can carry, in one place.
///
/// A struct, not nine positional arguments: the order used to be the only thing keeping
/// "memory" and "memory_index" apart, and the split below turns on knowing exactly which
/// sections are stable — a named field makes that a refactor, not a hunting trip through
/// call sites.
pub(crate) struct PromptParts<'a> {
    pub soul: &'a str,
    pub model: &'a str,
    pub provider: &'a str,
    pub memory: &'a str,
    pub memory_index: Option<&'a str>,
    /// Every skill's name plus its one-line description — small enough to ride along on
    /// every turn, so the model **chooses** a skill instead of hoping a keyword hits.
    pub skill_index: Option<&'a str>,
    /// The bodies of the skills whose trigger fired this turn (`$name` or a keyword match).
    pub skills: &'a str,
    pub todo: Option<&'a str>,
    pub summary: Option<&'a str>,
    /// The repository's own conventions (`AGENTS.md` upwards from the working directory).
    /// Sits in the dynamic half: it changes when the human switches project, not per turn,
    /// but it must not be part of the cached stable prefix either.
    pub instructions: Option<&'a str>,
    /// What this machine **cannot** do right now (no shell, no file writes). Said once per
    /// turn, in the prompt, because the alternative is the model discovering it by trial:
    /// a turn asked to clean up calendar events had no shell and no delete tool, so it spent
    /// eight iterations reading this repository's docs to find out where events live.
    pub capabilities: Option<&'a str>,
}

/// The system prompt cut in two at the cache boundary.
///
/// `stable` is byte-identical for the whole session (soul + model identity); `tail` is
/// everything that moves — the day, the todo list, memory hits, the skill catalog, the
/// rolling summary. Providers that want an explicit cache boundary (Anthropic) mark the
/// end of `stable`; the ones with automatic prefix caching simply need `stable` never to
/// move. Putting the clock in `tail` is the whole reason it is day-granular there: a
/// minute-level timestamp would invalidate the prefix sixty times an hour.
pub(crate) struct SystemPrompt {
    pub stable: String,
    pub tail: String,
}

impl SystemPrompt {
    /// One string, prefix first — for anyone who only wants to assert on the whole text
    /// (`full_turn` and the subagent pass the two halves through separately, so the cache
    /// boundary survives).
    #[cfg(test)]
    pub fn full(&self) -> String {
        if self.tail.is_empty() {
            self.stable.clone()
        } else {
            format!("{}\n{}", self.stable, self.tail)
        }
    }
}

/// What this machine cannot do right now, phrased as an instruction.
///
/// Two switches decide it, and both are off by default: `JOY_EXEC` (shell) and `JOY_WRITE`
/// (files). Without a line like this the model has no way to tell "the tool is missing" from
/// "I have not found the right tool yet" — it reads directories, greps docs and spends the
/// budget. Both Claude Code and Codex put their permission scope in the prompt for the same
/// reason; the difference is that theirs usually says *what is allowed*.
///
/// `None` when nothing is missing: the common case stays free.
pub(crate) fn capability_note(
    settings: &joyczl_config::Settings,
    tools_available: bool,
) -> Option<String> {
    if !tools_available {
        return Some("No tools are available for this turn. Tool references elsewhere do not grant access. Reply in plain text using the supplied context; explain when missing tools prevent verification. Do not emit tool-call syntax or claim a lookup or action occurred.".into());
    }
    let mut missing: Vec<&str> = Vec::new();
    if !settings.exec_enabled {
        missing.push("- running commands (shell) — off until the user sets `JOY_EXEC=1`");
    }
    if !settings.write_enabled {
        missing.push("- writing or editing files — off until the user sets `JOY_WRITE=1`");
    }
    if missing.is_empty() {
        return None;
    }
    let mut note = format!(
"What this machine cannot do right now:\n{}\nIf a request needs one of these, say so in one sentence and stop — do not approximate it with the tools you do have, and do not go reading source code to find a workaround.",
        missing.join("\n")
    );
    // The note used to read as if "no shell" meant "cannot look around", and the model said so
    // out loud: "I have no way to search the repository". Both searches are built in, so it is
    // worth one sentence to keep the two apart.
    if !settings.exec_enabled {
        note.push_str(
            "\nSearching is not on that list: `glob` finds files by name and `grep` finds lines \
             by content, and neither needs a shell.",
        );
    }
    Some(note)
}

pub(crate) fn build_system(parts_in: PromptParts<'_>) -> SystemPrompt {
    let PromptParts {
        soul,
        model,
        provider,
        memory,
        memory_index,
        skill_index,
        skills,
        todo,
        summary,
        instructions,
        capabilities,
    } = parts_in;
    let now = Local::now();
    // Stable half: what Joy is and what it runs on. Nothing here depends on the turn.
    let stable = format!(
"{soul}\nYour model: you are running on '{model}' via the '{provider}' provider, inside Joy, a local-first open-source agent harness."
    );

    // Dynamic half, in the order the model should read it: what time it is, what this run
    // is doing, what it remembers, what it could load.
    //
    // The clock is day-granular on purpose. `current_time` answers "what time is it" exactly;
    // a minute-stamped system prompt would break the cache boundary every turn to say
    // something the tool says better.
    let mut parts = vec![
        // The agent runs on the user's machine, so it should know the user machine's clock —
        // include the timezone name, or "in 30 minutes" has no answer.
        format!(
            "\nRight now it is {} ({}, UTC{})",
            now.format("%A, %Y-%m-%d"),
            now.format("%Z"),
            now.format("%:z")
        ),
    ];
    // Capability scope first: what is *missing* shapes every plan that follows. Codex puts its
    // sandbox permissions in the prompt for the same reason — a model that knows "no shell here"
    // says so in one sentence instead of burning a turn finding out.
    if let Some(capabilities) = capabilities {
        parts.push(format!("\n{capabilities}"));
    }
    // The todo list comes before memory and methods: it is **what this run is doing**, which should be seen
    // before background knowledge. And it is injected, not in history — compaction and sliding windows cannot wash it away.
    if let Some(todo) = todo {
        parts.push(format!("\n{todo}"));
    }
    if !memory.is_empty() {
        parts.push(format!("\nRelevant memory:\n{memory}"));
    }
    // The library's catalog: one line per page (title · kind — one-line summary), bodies on
    // demand via `read_memory_page`. This is the index layer: a new session carries the
    // table of contents, not the encyclopedia. Capped, because a catalog nobody prunes
    // becomes the context problem it was meant to solve.
    if let Some(index) = memory_index {
        if !index.trim().is_empty() {
            parts.push(format!(
                "\nMemory library (catalog only — call read_memory_page for a page):\n{index}"
            ));
        }
    }
    // The skill catalog always rides along; the bodies only for the skills this message
    // reached. Same rule as the memory catalog: the table of contents, not the book.
    if let Some(index) = skill_index {
        if !index.trim().is_empty() {
            parts.push(format!(
                "\nSkills (catalog only — call read_skill for a body when one fits):\n{index}"
            ));
        }
    }
    if !skills.is_empty() {
        parts.push(format!("\nRelevant skill instructions:\n{skills}"));
    }
    // Project conventions before memory: "how this repo works" outranks "what happened
    // before", and both lose to the human's message (the section says so out loud).
    if let Some(instructions) = instructions {
        parts.push(instructions.to_string());
    }
    if let Some(section) = summary.and_then(joyczl_memory::compaction::summary_section) {
        parts.push(section);
    }
    SystemPrompt {
        stable,
        tail: parts.join("\n"),
    }
}
