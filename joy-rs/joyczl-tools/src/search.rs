//! Searching: `glob` (find files by name) and `grep` (find lines by content).
//!
//! Split out of `fs.rs` when that module doubled: reading one file and walking a whole tree are
//! different concerns, and the walker below is the only thing here that has to think about
//! directories at all.
//!
//! Both tools exist because their absence had a cost: without them "find the file that holds X"
//! was a `list_dir` staircase, and on a machine with `JOY_EXEC` off the honest answer to "search
//! for it" was "I have no way to search" — the least defensible sentence an agent can say about
//! its own working directory. Neither needs a shell: they walk the tree themselves.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::fs::{expand_tilde, resolve_against_cwd};
use crate::Tool;

// ---- glob / grep: finding things without a shell -------------------------------------------

/// Directories a recursive search never walks into.
///
/// Build output and dependency trees dwarf the code in any real project: a `grep` that returns
/// forty thousand hits from `node_modules` is worse than no `grep` at all. The list is fixed
/// rather than configurable — a switch here would be one more thing to get wrong, and the tools
/// say what they skip, so a model that really needs to look inside `target/` can fall back to
/// `list_dir` + `read_file` (or `run_command`, which has its own gate).
const SEARCH_SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "vendor",
    ".next",
    ".turbo",
    ".cache",
    ".joy",
];

/// Filesystem entries one search may visit before it stops. A tree big enough to blow past this
/// is being searched from the wrong root; saying so beats walking for a minute.
const SEARCH_MAX_VISITS: usize = 20_000;

/// Matches `glob` and `grep` return before they stop. Same reasoning as [`READ_MAX_LINES`]:
/// the model needs the shape of the answer, not the whole thing — it can narrow the query.
pub(crate) const SEARCH_MAX_MATCHES: usize = 200;

/// Files larger than this are not searched: minified bundles, logs and sqlite blobs are not
/// where the answer to "where is this defined" lives, and reading them costs more than it gives.
const GREP_MAX_FILE_BYTES: u64 = 2_000_000;

/// Characters kept per matched line. One line of a minified file can be a megabyte; the
/// interesting part is where the match starts, not the rest of the line.
const GREP_MAX_LINE_CHARS: usize = 240;

/// Depth-first file walk, sorted at every level so two runs on one tree agree.
///
/// `visit` gets each **file** and returns `false` to stop the walk early (a cap was hit).
/// Returns whether it stopped early. Symlinks are skipped: a link into `/` would walk the disk
/// twice, and saying nothing about it is cheaper than explaining a sixty-second tool call.
fn walk_files(root: &Path, visit: &mut impl FnMut(&Path) -> bool) -> bool {
    let mut stack = vec![root.to_path_buf()];
    let mut visits = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(reader) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = reader.filter_map(Result::ok).collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            visits += 1;
            if visits > SEARCH_MAX_VISITS {
                return true;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                let name = entry.file_name();
                if SEARCH_SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
                    continue;
                }
                stack.push(entry.path());
            } else if file_type.is_file() && !visit(&entry.path()) {
                return true;
            }
        }
    }
    false
}

/// `*` and `?` inside one path segment — the usual recursive matcher, written out because
/// pulling in a glob crate for forty lines would cost more than it saves.
fn wildcard(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some(b'*') => (0..=text.len()).any(|skip| wildcard(&pattern[1..], &text[skip..])),
        Some(b'?') => !text.is_empty() && wildcard(&pattern[1..], &text[1..]),
        Some(&byte) => text.first() == Some(&byte) && wildcard(&pattern[1..], &text[1..]),
    }
}

fn match_segments(pattern: &[&str], text: &[&str]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        // `**` spans any number of segments, including none: `**/*.rs` has to match `a.rs` too.
        Some(&"**") => (0..=text.len()).any(|skip| match_segments(&pattern[1..], &text[skip..])),
        Some(segment) => {
            !text.is_empty()
                && wildcard(segment.as_bytes(), text[0].as_bytes())
                && match_segments(&pattern[1..], &text[1..])
        }
    }
}

/// Match a pattern against a path relative to the search root.
///
/// A pattern with no `/` is matched against the **file name** at any depth — `*.rs` is what a
/// model writes when it means "every Rust file", and requiring `**/*.rs` for that is a trap.
pub(crate) fn glob_match(pattern: &str, relative: &str) -> bool {
    let pattern = pattern.trim_start_matches("./");
    let relative = relative.trim_start_matches("./");
    if pattern.contains('/') {
        let segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
        let text: Vec<&str> = relative.split('/').filter(|s| !s.is_empty()).collect();
        return match_segments(&segments, &text);
    }
    let name = relative.rsplit('/').next().unwrap_or(relative);
    wildcard(pattern.as_bytes(), name.as_bytes())
}

/// Resolve a search root the same way the other read tools do (`~`, then the turn's cwd).
fn search_root(ctx: &crate::ToolCtx, args: &Value) -> Result<(String, PathBuf)> {
    let raw = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(".");
    let root = resolve_against_cwd(ctx, expand_tilde(raw));
    if !root.is_dir() {
        return Err(anyhow!(
            "{raw} is not a directory (search a directory; read a file with read_file)."
        ));
    }
    Ok((raw.to_string(), root))
}

/// How a found path is shown: relative to the root, so a hundred results stay readable.
fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn glob_tool() -> Tool {
    Tool {
        name: "glob".to_string(),
        description: "Find files by name pattern. `**` spans directories, `*` and `?` stay inside one, and a pattern with no `/` matches the file name at any depth (`*.rs` = every Rust file). Skips .git, node_modules, target, dist, build and other dependency trees. Use it to locate a file; use grep to find where something is written."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The pattern, e.g. `**/*.ts`, `src/**/mod.rs`, `*.toml`."
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search from (default: the working directory). ~ is expanded."
                }
            },
            "required": ["pattern"]
        }),
        handler: Arc::new(|ctx, args| Box::pin(async move { execute_glob(&ctx, &args).await })),
    }
}

async fn execute_glob(ctx: &crate::ToolCtx, args: &Value) -> Result<String> {
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("glob needs a `pattern` (e.g. `**/*.rs`)."))?
        .to_string();
    let (raw, root) = search_root(ctx, args)?;
    let walk_root = root.clone();
    let matcher = pattern.clone();
    let (matches, truncated) = tokio::task::spawn_blocking(move || {
        let mut hits: Vec<String> = Vec::new();
        let cut = walk_files(&walk_root, &mut |path| {
            let relative = display_path(&walk_root, path);
            if !glob_match(&matcher, &relative) {
                return true;
            }
            hits.push(relative);
            hits.len() < SEARCH_MAX_MATCHES
        });
        (hits, cut)
    })
    .await
    .map_err(|e| anyhow!("Could not run the search: {e}"))?;

    if matches.is_empty() {
        return Ok(format!(
            "No file under {raw} matches `{pattern}`. (Searches skip .git, node_modules, target, \
             dist, build and similar trees.)"
        ));
    }
    let total = matches.len();
    let mut out = matches.join("\n");
    out.push_str(&format!("\n\n[{total} file(s)]"));
    if truncated {
        out.push_str(&format!(
            "\n[Stopped at {SEARCH_MAX_MATCHES} — narrow the pattern or the path.]"
        ));
    }
    Ok(out)
}

pub fn grep_tool() -> Tool {
    Tool {
        name: "grep".to_string(),
        description: "Search file contents and return `path:line: text` for every match. Matching is plain text, not a regex; for patterns this cannot express, run_command (JOY_EXEC=1) has the real thing. Skips .git, node_modules, target, dist, build, binary files and anything over 2 MB. Use it to find where something is defined or used."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The text to look for, e.g. `fn main` or `max_iterations`."
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search (default: the working directory). ~ is expanded."
                },
                "glob": {
                    "type": "string",
                    "description": "Only search files whose name matches this pattern, e.g. `*.rs`."
                },
                "ignore_case": {
                    "type": "boolean",
                    "description": "Match case-insensitively (default false)."
                }
            },
            "required": ["pattern"]
        }),
        handler: Arc::new(|ctx, args| Box::pin(async move { execute_grep(&ctx, &args).await })),
    }
}

async fn execute_grep(ctx: &crate::ToolCtx, args: &Value) -> Result<String> {
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("grep needs a `pattern` (the text to look for)."))?
        .to_string();
    let filter = args
        .get("glob")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let ignore_case = args
        .get("ignore_case")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (raw, root) = search_root(ctx, args)?;
    let walk_root = root.clone();
    let needle = if ignore_case {
        pattern.to_lowercase()
    } else {
        pattern.clone()
    };
    let (hits, truncated) = tokio::task::spawn_blocking(move || {
        let mut hits: Vec<String> = Vec::new();
        let cut = walk_files(&walk_root, &mut |path| {
            let relative = display_path(&walk_root, path);
            if let Some(filter) = &filter {
                if !glob_match(filter, &relative) {
                    return true;
                }
            }
            let Ok(meta) = std::fs::metadata(path) else {
                return true;
            };
            if meta.len() > GREP_MAX_FILE_BYTES {
                return true;
            }
            let Ok(bytes) = std::fs::read(path) else {
                return true;
            };
            // A NUL byte in the head is the usual tell for "not text" (`read_file` uses the same
            // signal). It has to be checked before decoding: NUL is perfectly valid UTF-8, so
            // `String::from_utf8` accepts a binary blob and the "matches" are then noise.
            if bytes.iter().take(8 * 1024).any(|byte| *byte == 0) {
                return true;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                // Not UTF-8: a binary is not where the answer is.
                return true;
            };
            let haystack = if ignore_case {
                text.to_lowercase()
            } else {
                text.as_str().to_string()
            };
            if !haystack.contains(&needle) {
                return true;
            }
            for (index, line) in text.lines().enumerate() {
                let candidate = if ignore_case {
                    line.to_lowercase()
                } else {
                    line.to_string()
                };
                if !candidate.contains(&needle) {
                    continue;
                }
                let shown: String = line.chars().take(GREP_MAX_LINE_CHARS).collect();
                hits.push(format!("{relative}:{}: {}", index + 1, shown.trim_end()));
                if hits.len() >= SEARCH_MAX_MATCHES {
                    return false;
                }
            }
            true
        });
        (hits, cut)
    })
    .await
    .map_err(|e| anyhow!("Could not run the search: {e}"))?;

    if hits.is_empty() {
        return Ok(format!(
            "No match for `{pattern}` under {raw}. (Searches skip .git, node_modules, target, \
             dist, build, binary files and anything over 2 MB.)"
        ));
    }
    let total = hits.len();
    let mut out = hits.join("\n");
    out.push_str(&format!("\n\n[{total} match(es)]"));
    if truncated {
        out.push_str(&format!(
            "\n[Stopped at {SEARCH_MAX_MATCHES} — narrow the pattern, the path or the glob.]"
        ));
    }
    Ok(out)
}
