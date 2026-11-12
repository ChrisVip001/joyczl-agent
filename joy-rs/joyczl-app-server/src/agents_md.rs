//! Project instructions: `AGENTS.md` and friends, from the working directory upwards.
//!
//! A repository's conventions ("use pnpm, not npm"; "tests go in `__tests__`") are not
//! something the human should have to repeat every session, and not something a model can
//! infer from the file tree. Codex and pi both read a project file; Joy read only its own
//! soul, which made it a competent stranger in every repository.
//!
//! The rule, deliberately small:
//!
//! * walk up from the working directory until a `.git` is found (inclusive), or the disk root;
//! * in each directory take **one** file, in this order: `AGENTS.override.md`, `AGENTS.md`,
//!   `CLAUDE.md` — the same preference codex uses, so the two agents read the same file in a
//!   repo that has both;
//! * concatenate from the outermost directory inwards (broad conventions first, local
//!   specifics last — last read wins when they disagree);
//! * cap the total: the innermost file matters most, so the budget is filled from the inside
//!   out and the outer files are dropped when it runs out.
//!
//! It goes into the **dynamic** half of the system prompt, after the clock and capabilities:
//! not into the stable prefix, or every turn would break the provider's prompt cache (these
//! files change rarely, but the cache break would be paid on every turn).

use std::path::{Path, PathBuf};

/// Files looked for in each directory, most specific first.
const NAMES: &[&str] = &["AGENTS.override.md", "AGENTS.md", "CLAUDE.md"];

/// Total budget for all files. A 200 KB `AGENTS.md` would be the context problem it was
/// meant to solve; the tail of it is cut, and the cut is labelled.
const MAX_BYTES: usize = 32 * 1024;

/// Walk up from `cwd`, collect project instruction files, return the prompt section.
///
/// `None` when there is nothing to say — the common case (no file anywhere), and it must cost
/// nothing: no section, no marker, no extra tokens.
pub fn discover(cwd: &Path) -> Option<String> {
    let files = collect(cwd);
    if files.is_empty() {
        return None;
    }

    // Fill from the inside out: if the budget runs out, the file nearest the working
    // directory survives and the broad one is dropped.
    let mut kept: Vec<(PathBuf, String)> = Vec::new();
    let mut used = 0usize;
    // `collect` already returns innermost-first: fill in that order, reverse for the prompt.
    for path in files.iter() {
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        let text = raw.trim();
        if text.is_empty() {
            continue;
        }
        if used + text.len() > MAX_BYTES {
            if kept.is_empty() {
                // The innermost file alone is over budget: keep the head, say so.
                let head = truncate(text, MAX_BYTES);
                kept.push((
                    path.clone(),
                    format!("{head}\n…（超过 {MAX_BYTES} 字节，后面截掉了）"),
                ));
            }
            // Anything further out would only add less-local conventions on top of a
            // truncated pointer: drop it.
            break;
        }
        used += text.len();
        kept.push((path.clone(), text.to_string()));
    }

    if kept.is_empty() {
        return None;
    }
    kept.reverse();

    let mut out = String::from(
        "\n项目约定（从这个仓库里读到的，按它干活；与用户这次的要求冲突时以用户为准）：",
    );
    for (path, text) in kept {
        out.push_str(&format!("\n\n来自 {}：\n{text}", path.display()));
    }
    Some(out)
}

/// One file per directory, innermost first (callers reverse it).
fn collect(cwd: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut here = Some(cwd);
    while let Some(dir) = here {
        if let Some(path) = NAMES
            .iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file())
        {
            found.push(path);
        }
        // `.git` marks the repository root: conventions above it belong to some other
        // project (or the user's home), not to this workspace.
        if dir.join(".git").exists() {
            break;
        }
        here = dir.parent();
    }
    found
}

/// Cut on a character boundary — a Chinese character must not become invalid UTF-8.
fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
#[path = "agents_md_tests.rs"]
mod agents_md_tests;
