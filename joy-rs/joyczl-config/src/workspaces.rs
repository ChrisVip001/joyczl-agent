//! `<home>/workspaces.json` — recently used dirs (quick entries for the cockpit workdir picker).
//!
//! dsh's workspace is a domain record (uuid + realpath + session ownership); joy's cwd is
//! already a per-turn parameter, not persisted, so this keeps only its most useful bit:
//! **"remember used directories for one-click access next time"** — a convenience, not a
//! source of truth; a broken file or failed write must never drag down any turn.
//!
//! Three disciplines:
//! * **canonicalize dedup** (dsh's realpath matching): the same directory under a different
//!   symlink spelling or trailing slash produces no duplicate entry.
//! * **Move hits to front**: most recently used on top.
//! * **Cap at 8 entries**: this is a shortcut list, not a history ledger.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How many entries to keep at most.
const MAX_PATHS: usize = 8;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorkspacesFile {
    #[serde(default)]
    pub paths: Vec<String>,
}

pub fn workspaces_path(home: &Path) -> PathBuf {
    home.join("workspaces.json")
}

/// Read. Missing or broken file — always an empty list (fail-open, same discipline as routes).
pub fn load_workspaces(home: &Path) -> WorkspacesFile {
    std::fs::read_to_string(workspaces_path(home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Record an entry. canonicalize dedup (on failure keep as-is — nonexistent still counts as
/// "want to go"), move hits to front, cap; silent on write failure — convenience never alarms.
pub fn record_workspace(home: &Path, raw: &str) {
    let canonical = std::fs::canonicalize(raw).unwrap_or_else(|_| PathBuf::from(raw));
    let path = canonical.to_string_lossy().to_string();
    let mut file = load_workspaces(home);
    file.paths.retain(|p| p != &path);
    file.paths.insert(0, path);
    file.paths.truncate(MAX_PATHS);
    let _ = std::fs::write(
        workspaces_path(home),
        serde_json::to_string_pretty(&file).unwrap_or_default(),
    );
}

#[cfg(test)]
mod workspaces_tests {
    use super::*;

    #[test]
    fn missing_or_broken_file_means_empty() {
        let dir = tempfile::tempdir().expect("临时目录");
        assert!(load_workspaces(dir.path()).paths.is_empty());
        std::fs::write(workspaces_path(dir.path()), "{ 坏的").unwrap();
        assert!(load_workspaces(dir.path()).paths.is_empty());
    }

    #[test]
    fn recording_dedupes_moves_to_front_and_caps() {
        let dir = tempfile::tempdir().expect("临时目录");
        let a = dir.path().join("甲");
        let b = dir.path().join("乙");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();

        // Expected values also go through canonicalize — macOS tempdir lives behind a
        // symlink (/var → /private/var), and what is recorded is the canonical form.
        let ca = std::fs::canonicalize(&a)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let cb = std::fs::canonicalize(&b)
            .unwrap()
            .to_string_lossy()
            .to_string();

        record_workspace(dir.path(), &a.to_string_lossy());
        // Same directory, different spelling (trailing slash): canonicalize dedups, no second entry.
        record_workspace(dir.path(), &format!("{}/", a.display()));
        assert_eq!(load_workspaces(dir.path()).paths.len(), 1);

        record_workspace(dir.path(), &b.to_string_lossy());
        assert_eq!(load_workspaces(dir.path()).paths, vec![cb, ca.clone()]);

        // Record a again: moved to front.
        record_workspace(dir.path(), &a.to_string_lossy());
        assert_eq!(load_workspaces(dir.path()).paths[0], ca);
    }

    #[test]
    fn the_list_is_capped() {
        let dir = tempfile::tempdir().expect("临时目录");
        for i in 0..(MAX_PATHS + 3) {
            let p = dir.path().join(format!("目录{i}"));
            std::fs::create_dir(&p).unwrap();
            record_workspace(dir.path(), &p.to_string_lossy());
        }
        assert_eq!(load_workspaces(dir.path()).paths.len(), MAX_PATHS);
    }

    /// A nonexistent directory is still recorded (canonicalize failure keeps as-is) — "want
    /// to go but not built yet" is legitimate intent; the picker will say so in plain words.
    #[test]
    fn a_missing_directory_is_still_recorded() {
        let dir = tempfile::tempdir().expect("临时目录");
        let ghost = dir.path().join("还没建");
        record_workspace(dir.path(), &ghost.to_string_lossy());
        assert_eq!(
            load_workspaces(dir.path()).paths,
            vec![ghost.to_string_lossy().to_string()]
        );
    }
}
