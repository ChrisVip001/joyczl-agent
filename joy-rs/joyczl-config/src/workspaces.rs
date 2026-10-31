//! `<home>/workspaces.json` —— 最近用过的目录（驾驶舱选工作目录的快捷入口）。
//!
//! dsh 的 workspace 是一条领域记录（uuid + realpath + 会话归属）；joy 的
//! cwd 本来就是每轮参数、不持久，所以这里只取它最有用的那一小块：
//! **「用过的目录记住，下次一键直达」**。这是便利品，不是事实源 ——
//! 文件坏了、写失败了，都不该连累任何一轮对话。
//!
//! 三条纪律：
//! * **canonicalize 去重**（dsh 的 realpath 判重）：同一个目录换了符号链
//!   写法或尾部斜杠，不产生两条记录。
//! * **命中移前**：最近用的在最上面。
//! * **封顶 8 条**：这是快捷方式列表，不是历史账本。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 最多记几条。
const MAX_PATHS: usize = 8;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorkspacesFile {
    #[serde(default)]
    pub paths: Vec<String>,
}

pub fn workspaces_path(home: &Path) -> PathBuf {
    home.join("workspaces.json")
}

/// 读。文件不存在、损坏 —— 一律空列表（失败开放，同 routes 的纪律）。
pub fn load_workspaces(home: &Path) -> WorkspacesFile {
    std::fs::read_to_string(workspaces_path(home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// 记一笔。canonicalize 去重（失败就用原样 —— 目录不存在也算「想去」）、
/// 命中移前、封顶；写失败静默 —— 便利品没有资格报警。
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

        // 期望值也走 canonicalize —— macOS 的 tempdir 住在符号链后面
        // （/var → /private/var），记录的是规范形式。
        let ca = std::fs::canonicalize(&a)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let cb = std::fs::canonicalize(&b)
            .unwrap()
            .to_string_lossy()
            .to_string();

        record_workspace(dir.path(), &a.to_string_lossy());
        // 同一目录换个写法（尾部斜杠）：canonicalize 判重，不产生第二条。
        record_workspace(dir.path(), &format!("{}/", a.display()));
        assert_eq!(load_workspaces(dir.path()).paths.len(), 1);

        record_workspace(dir.path(), &b.to_string_lossy());
        assert_eq!(load_workspaces(dir.path()).paths, vec![cb, ca.clone()]);

        // 再记 a：移到最前。
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

    /// 不存在的目录也照记（canonicalize 失败用原样）——「想去但还没建」
    /// 是合法意图，选择器那边点了自然会得到一句人话。
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
