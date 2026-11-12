//! Reading a directory of JSONL cases — **one** copy.
//!
//! Three loaders used to do this by hand (deterministic, judge, replay). The copies had drifted
//! exactly where it hurts: one had no 文件:行号 in its errors, two did not sort, so the same suite
//! printed its table in a different order on a different machine — which reads as "a case
//! changed" when nothing changed.
//!
//! What the format is: one JSON object per line, `#` lines and blanks ignored, directories mean
//! "every `*.jsonl` in here". Everything else is the caller's business, which is why `check`
//! exists: a case may be valid JSON and still be unusable (an unknown `schemaVersion`), and that
//! judgement belongs to the suite, not to the reader.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;

/// Reads every case under `paths`; a path may be a file or a directory of `*.jsonl`.
///
/// `check` gets the case and a human-readable location (`<file> 第 N 行`), so a refusal can say
/// where the offender is without re-deriving it.
pub fn read_jsonl<T>(
    paths: &[PathBuf],
    label: &str,
    check: impl Fn(&T, &str) -> Result<()>,
) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    let mut cases = Vec::new();
    for path in paths {
        for file in files_of(path, label)? {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("读不了{label}文件：{}", file.display()))?;
            for (index, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let at = format!("{} 第 {} 行", file.display(), index + 1);
                let case: T = serde_json::from_str(line)
                    .map_err(|e| anyhow::anyhow!("{at} 不是合法{label}：{e}"))?;
                check(&case, &at)?;
                cases.push(case);
            }
        }
    }
    Ok(cases)
}

/// A file stays a file; a directory contributes its `*.jsonl`, **sorted** (stable table order).
fn files_of(path: &Path, label: &str) -> Result<Vec<PathBuf>> {
    if !path.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(path)
        .with_context(|| format!("读不了{label}目录：{}", path.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    Ok(files)
}
