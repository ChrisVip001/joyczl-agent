//! **Spilling** oversized text to disk, plus **a preview that actually fits**.
//!
//! Two callers, one set of rules: `exec.rs` (command output over 8000 chars) and
//! the in-turn tool-result budget in `joyczl-loop` (a batch over the threshold).
//!
//! Three invariants:
//!
//! 1. **The preview must really fit**: reserve the worst-case length of the omission notice
//!    first, then budget head/tail — if it still overflows after the swap, the stub is pointless.
//! 2. **Cut at whole lines only**: half a line (half a JSON, half a log entry) reads worse
//!    than one fewer line, and the model may take it as everything — say so honestly then.
//! 3. **A failed spill doesn't affect the result**: the stub still stands (just no path to
//!    look up); never turn a successful call into an error because the write failed.

use std::path::Path;

/// The stub after spilling: replacement text + where the full original lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stub {
    /// The replacement text, **at most as many chars as the caller's budget**.
    pub text: String,
    /// Path of the full original relative to home (`None` if spilling failed or no dir given).
    pub path: Option<String>,
    /// How many chars and lines were omitted (for tests and logs).
    pub omitted_chars: usize,
    pub omitted_lines: usize,
}

/// Writes the full original to `<dir>/<date>/<tag>-<time>-<pid>.txt`; returns the **path relative to home**.
///
/// A relative path reads well and keeps the user's absolute paths out of the context. Any
/// failing step returns `None` — the caller continues as "nothing was spilled".
pub fn spill_text(dir: &Path, text: &str, tag: &str) -> Option<String> {
    let now = chrono::Local::now();
    let day = now.format("%Y%m%d").to_string();
    let stamp = now.format("%H%M%S%.3f").to_string();
    let folder = dir.join(&day);
    std::fs::create_dir_all(&folder).ok()?;
    let file = folder.join(format!("{tag}-{stamp}-{}.txt", std::process::id()));
    std::fs::write(&file, text).ok()?;
    // Relative to home (spill's parent): reads as "go look under spill/…".
    //
    // Separator normalized to `/`: this string is **written for the model**, and on Windows
    // `display()` gives backslashes — the same sentence looks different per platform, and
    // tests fork into two sets. A unified `/` still joins back on Windows (both are accepted).
    let relative = dir
        .parent()
        .and_then(|home| file.strip_prefix(home).ok())
        .unwrap_or(&file);
    Some(relative.display().to_string().replace('\\', "/"))
}

/// Build a fitting stub for oversized text. Returns `None` if the text is already within budget.
///
/// With `dir = None` nothing is spilled (so no path appears in the stub).
pub fn stub(dir: Option<&Path>, text: &str, budget_chars: usize, tag: &str) -> Option<Stub> {
    let total = text.chars().count();
    if total <= budget_chars || budget_chars == 0 {
        return None;
    }

    // Spill the **full original** first — before cutting (reversed order would save the cut copy).
    let path = dir.and_then(|dir| spill_text(dir, text, tag));

    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    // Reserve the omission notice at a worst-case fixed length: compute an upper bound first
    // (lines and chars at full size), then budget head/tail. Shorter is fine — **never over budget**.
    let notice = notice_for(total_lines, total, path.as_deref());
    let notice_len = notice.chars().count();
    if notice_len + 2 >= budget_chars {
        // Budget too small even for the notice: keep just the notice, saying honestly this is all.
        let mut text = notice;
        text.truncate_chars(budget_chars);
        return Some(Stub {
            text,
            path,
            omitted_chars: total,
            omitted_lines: total_lines,
        });
    }

    let remaining = budget_chars - notice_len - 2; // two newlines
    let head_budget = remaining.div_ceil(2);
    let tail_budget = remaining / 2;

    let (head, head_lines, head_chars) = take_head_lines(&lines, head_budget);
    let (tail, tail_lines, tail_chars) = take_tail_lines(&lines, tail_budget);

    let omitted_lines = total_lines.saturating_sub(head_lines + tail_lines);
    let omitted_chars = total.saturating_sub(head_chars + tail_chars);
    let notice = notice_for(omitted_lines, total, path.as_deref());

    let mut out = String::new();
    if !head.is_empty() {
        out.push_str(&head);
        out.push('\n');
    }
    out.push_str(&notice);
    if !tail.is_empty() {
        out.push('\n');
        out.push_str(&tail);
    }
    debug_assert!(
        out.chars().count() <= budget_chars.max(notice_len + 2),
        "桩必须放得下：{} > {budget_chars}",
        out.chars().count()
    );

    Some(Stub {
        text: out,
        path,
        omitted_chars,
        omitted_lines,
    })
}

/// The omission notice. Two wordings, with and without a path — the latter must never say
/// "full output at" pointing at a file that doesn't exist.
fn notice_for(omitted_lines: usize, total_chars: usize, path: Option<&str>) -> String {
    match path {
        Some(path) => format!(
            "…（结果共 {total_chars} 字符，已截断，省略了 {omitted_lines} 行；完整输出在 {path}）"
        ),
        // No spill means **no path mention** — "full output at …" without the file is worse than silence.
        None => format!("…（结果共 {total_chars} 字符，已截断，省略了 {omitted_lines} 行）"),
    }
}

/// Take whole lines from the start, until the budget is nearly exceeded.
fn take_head_lines(lines: &[&str], budget: usize) -> (String, usize, usize) {
    let mut out = String::new();
    let mut used = 0usize;
    let mut taken = 0usize;
    for line in lines {
        let cost = line.chars().count() + 1; // the newline counts too
        if used + cost > budget {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        used += cost;
        taken += 1;
    }
    // Not a single line fit: return an honest blank so the caller keeps only the omission notice.
    (out, taken, used)
}

/// Take whole lines from the end, until the budget is nearly exceeded.
fn take_tail_lines(lines: &[&str], budget: usize) -> (String, usize, usize) {
    let mut picked: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in lines.iter().rev() {
        let cost = line.chars().count() + 1;
        if used + cost > budget {
            break;
        }
        picked.push(line);
        used += cost;
    }
    picked.reverse();
    let taken = picked.len();
    (picked.join("\n"), taken, used)
}

/// Truncate by char count (only when the budget can't even fit the notice).
trait TruncateChars {
    fn truncate_chars(&mut self, max: usize);
}

impl TruncateChars for String {
    fn truncate_chars(&mut self, max: usize) {
        if self.chars().count() <= max {
            return;
        }
        *self = self.chars().take(max).collect();
    }
}
