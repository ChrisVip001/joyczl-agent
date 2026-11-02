//! 驾驶舱前端的「新鲜度」检查（M2 收尾补的）。
//!
//! dist 是构建产物且被 gitignore —— 没有任何机制逼人重建，于是出现过
//! 「源码更新了一个多月、端出来的页面还是 9 月的」这种最隐蔽的坏：服务
//! 正常 200，用户面对的却是一年前的 UI。修法对齐守护锁同一条哲学：
//! **配错（前端过期）就该起不来**，而不是默默端出旧页面。
//!
//! 只在源码目录存在时才检查（发布/安装场景只有 dist 没有 joy-ts，不检查
//! —— 那时候没有「更新鲜的源码」可言）。

use std::path::Path;
use std::time::SystemTime;

/// 源码目录：与 `workspace_assets()` 同一条相对路径。
pub(crate) fn dashboard_src_dir(dist: &Path) -> Option<std::path::PathBuf> {
    // …/joy-ts/packages/dashboard/dist → …/joy-ts/packages/dashboard/src
    dist.parent().map(|pkg| pkg.join("src"))
}

/// dist 比 src 旧就报错。返回 Err(说明文案) —— 调用方直接 bail。
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
        return Ok(()); // 发布场景：没有源码可比较，不检查。
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

/// 目录下（递归）最新的 mtime。读不了的文件跳过 —— 新鲜度是尽力而为的
/// 提示，不是审计。
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

    /// 源码比 dist 新 → 报错，且文案里有重建命令。
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

    /// dist 比源码新（或一样新）→ 放行；没有 src 目录（发布场景）→ 放行。
    #[test]
    fn fresh_or_sourceless_passes() {
        let dir = tempfile::tempdir().expect("临时目录");
        let dist = dir.path().join("packages/dashboard/dist");
        let src = dir.path().join("packages/dashboard/src");
        touch(&src.join("main.ts"));
        std::thread::sleep(Duration::from_millis(20));
        touch(&dist.join("main.js"));
        check_dashboard_freshness(&dist).expect("新的 dist 该放行");

        // 没有 dist → 另一条错误（没构建），不是「过期」。
        let empty = tempfile::tempdir().expect("临时目录");
        let error = check_dashboard_freshness(empty.path()).unwrap_err();
        assert!(error.contains("还没构建"), "{error}");

        // 没有 src（发布场景）→ 放行，哪怕 dist 很老。
        let lonely = tempfile::tempdir().expect("临时目录");
        touch(&lonely.path().join("dist/main.js"));
        check_dashboard_freshness(&lonely.path().join("dist")).expect("无源码不检查");
    }
}
