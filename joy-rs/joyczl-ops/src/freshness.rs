//! "Freshness" check for the dashboard frontend (added to close out M2).
//!
//! dist is a build artifact and gitignored — nothing forces a rebuild, and so the sneakiest
//! failure appeared: "source updated a month ago, the served page is still from September".
//! The service returns 200 while the user faces a year-old UI. The fix follows the guard
//! lock's philosophy: **a misconfiguration (stale frontend) must fail to start**, not serve an old page.
//!
//! Only checked when the source directory exists (in release/install scenes there is dist
//! but no joy-ts; skip — there is no "fresher source" to compare against).

use std::path::Path;
use std::time::SystemTime;

/// Source directory: same relative path as `workspace_assets()`.
pub(crate) fn dashboard_src_dir(dist: &Path) -> Option<std::path::PathBuf> {
    // …/joy-ts/packages/dashboard/dist → …/joy-ts/packages/dashboard/src
    dist.parent().map(|pkg| pkg.join("src"))
}

/// Error when dist is older than src. Returns Err(explanation) — the caller just bails.
pub(crate) fn check_dashboard_freshness(dist: &Path) -> Result<(), String> {
    let built = match std::fs::metadata(dist.join("main.js")) {
        Ok(meta) => meta
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH),
        Err(_) => {
            return Err(format!(
                "驾驶舱前端还没构建：{} 不存在。跑 `just dashboard-build`（或 cd joy-ts && npm run build --workspace @joy/dashboard）",
                dist.display()
            ))
        }
    };
    let Some(src) = dashboard_src_dir(dist) else {
        return Ok(());
    };
    if !src.is_dir() {
        return Ok(()); // Release scene: no source to compare against; skip the check.
    }
    let newest_src = newest_mtime(&src).unwrap_or(SystemTime::UNIX_EPOCH);
    if newest_src > built {
        return Err(format!(
            "驾驶舱前端过期了：源码（{}）比构建产物（{}）新。跑 `just dashboard-build` 重建，或设 JOY_DASHBOARD_DIR 指向现成的 dist",
            src.display(),
            dist.display()
        ));
    }
    Ok(())
}

/// Newest mtime under a directory (recursive). Unreadable files are skipped — freshness is
/// a best-effort hint, not an audit.
fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                _ => {
                    if let Ok(meta) = entry.metadata() {
                        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                        if newest.is_none_or(|n| mtime > n) {
                            newest = Some(mtime);
                        }
                    }
                }
            }
        }
    }
    newest
}

#[cfg(test)]
mod freshness_tests {
    use super::*;
    use std::time::Duration;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).expect("建目录");
        std::fs::write(path, "x").expect("写文件");
    }

    /// Source newer than dist → error, with the rebuild command in the message.
    #[test]
    fn stale_dist_is_a_hard_error_with_the_fix() {
        let dir = tempfile::tempdir().expect("临时目录");
        let dist = dir.path().join("packages/dashboard/dist");
        let src = dir.path().join("packages/dashboard/src");
        touch(&dist.join("main.js"));
        std::thread::sleep(Duration::from_millis(20));
        touch(&src.join("main.ts"));

        let error = check_dashboard_freshness(&dist).unwrap_err();
        assert!(error.contains("just dashboard-build"), "{error}");
    }

    /// dist newer than (or equal to) source → pass; no src dir (release) → pass.
    #[test]
    fn fresh_or_sourceless_passes() {
        let dir = tempfile::tempdir().expect("临时目录");
        let dist = dir.path().join("packages/dashboard/dist");
        let src = dir.path().join("packages/dashboard/src");
        touch(&src.join("main.ts"));
        std::thread::sleep(Duration::from_millis(20));
        touch(&dist.join("main.js"));
        check_dashboard_freshness(&dist).expect("新的 dist 该放行");

        // No dist → the other error (not built), not "stale".
        let empty = tempfile::tempdir().expect("临时目录");
        let error = check_dashboard_freshness(empty.path()).unwrap_err();
        assert!(error.contains("还没构建"), "{error}");

        // No src (release scene) → pass, even if dist is old.
        let lonely = tempfile::tempdir().expect("临时目录");
        touch(&lonely.path().join("dist/main.js"));
        check_dashboard_freshness(&lonely.path().join("dist")).expect("无源码不检查");
    }
}
