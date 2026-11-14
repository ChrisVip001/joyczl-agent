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
                "the cockpit front end has not been built: {} does not exist. Run `just dashboard-build` (or cd joy-ts && npm run build --workspace @joy/dashboard)",
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
            "the cockpit front end is stale: the source ({}) is newer than the build ({}). Run `just dashboard-build`, or point JOY_DASHBOARD_DIR at an existing dist",
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
        std::fs::create_dir_all(path.parent().unwrap()).expect("create the directory");
        std::fs::write(path, "x").expect("write the file");
    }

    /// Source newer than dist → error, with the rebuild command in the message.
    #[test]
    fn stale_dist_is_a_hard_error_with_the_fix() {
        let dir = tempfile::tempdir().expect("temp dir");
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
        let dir = tempfile::tempdir().expect("temp dir");
        let dist = dir.path().join("packages/dashboard/dist");
        let src = dir.path().join("packages/dashboard/src");
        touch(&src.join("main.ts"));
        std::thread::sleep(Duration::from_millis(20));
        touch(&dist.join("main.js"));
        check_dashboard_freshness(&dist).expect("a fresh dist should pass");

        // No dist → the other error (not built), not "stale".
        let empty = tempfile::tempdir().expect("temp dir");
        let error = check_dashboard_freshness(empty.path()).unwrap_err();
        assert!(error.contains("has not been built"), "{error}");

        // No src (release scene) → pass, even if dist is old.
        let lonely = tempfile::tempdir().expect("temp dir");
        touch(&lonely.path().join("dist/main.js"));
        check_dashboard_freshness(&lonely.path().join("dist")).expect("no source, no check");
    }
}
