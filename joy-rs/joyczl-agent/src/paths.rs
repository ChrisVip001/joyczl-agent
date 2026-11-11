//! **The single containment rule of the whole repo**: `path` must land inside `root` after
//! symlink resolution.
//!
//! Two callers need exactly this rule — the write tools (may I write here?) and the task
//! verifier (may I read this acceptance condition?). Two copies of "is this path inside" would
//! drift, and the drift would be a security bug, not a cosmetic one. So the implementation lives
//! here, in the crate both sides depend on, and `joyczl_tools::fs_write` re-exports it.

use std::path::{Path, PathBuf};

/// **The single containment rule**: `path` must land inside `root` after symlink resolution.
///
/// The file itself may not exist yet (a create is not an escape), so the deepest **existing**
/// ancestor is canonicalized — every symlink that really exists in the prefix must resolve
/// inside the root. Returns `path` itself (the caller's path, now proven contained) rather than
/// the canonical ancestor: the caller wants to open/write the file it named.
pub fn inside_workspace(path: &Path, root: &Path) -> Result<PathBuf, String> {
    let root_real = root
        .canonicalize()
        .map_err(|e| format!("工作目录不可用（{}）：{e}", root.display()))?;
    let real = existing_ancestor(path);
    if real.starts_with(&root_real) {
        Ok(path.to_path_buf())
    } else {
        Err(format!("{path:?} 在工作目录（{}）之外", root.display()))
    }
}

/// Canonicalize the deepest existing ancestor of `path` — the file itself may not exist yet
/// (a create is not an escape), but every symlink in its real prefix must resolve inside.
fn existing_ancestor(path: &Path) -> PathBuf {
    let mut cur = path.to_path_buf();
    loop {
        match cur.canonicalize() {
            Ok(real) => return real,
            Err(_) => {
                if !cur.pop() {
                    return path.to_path_buf(); // nowhere to anchor; containment below refuses
                }
            }
        }
    }
}
