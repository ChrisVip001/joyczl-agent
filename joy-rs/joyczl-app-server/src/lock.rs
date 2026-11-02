//! 守护锁：同一个 `<home>` 只允许一个 app-server 进程。
//!
//! 用 PID 文件 + 操作锁：PID 文件防的是**真正的事故** —— 两个 app-server 各自持有一份
//! 内存态（审批、目标、作业表），写同一个 state.db，谁也不知道对方的存在。
//! 「已在跑」必须是启动期的硬错误，不是警告。
//!
//! 语义三条：
//! * 拿锁 = `O_EXCL` 建文件、写入自己的 PID；
//! * 文件已存在 → 读出 PID 查进程是否活着：活着就报错退出；死了就是
//!   上次崩溃的残骸，**清掉重试**（不让人手动删文件）；
//! * `Drop` 时删文件。崩溃清不掉没关系 —— 下一个进程会按上面的规则收尸。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 锁文件名（放在 `<home>` 下，与其他状态文件同一个约定）。
const LOCK_FILE: &str = "app-server.lock";

/// 守护锁。持有期间锁文件存在；`Drop` 删除。
#[derive(Debug)]
pub struct DaemonLock {
    path: PathBuf,
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 尝试为 `home` 拿守护锁。拿不到就返回带原因的错误（含在跑的 PID 与文件路径）。
pub fn acquire(home: &Path) -> Result<DaemonLock, String> {
    let path = home.join(LOCK_FILE);
    for attempt in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                write!(file, "{}", std::process::id()).map_err(|e| {
                    let _ = std::fs::remove_file(&path);
                    format!("锁文件写不进去（{}）：{e}", path.display())
                })?;
                return Ok(DaemonLock { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // 已有锁：进程活着就是真冲突；死了就是崩溃残骸，收尸重试。
                let pid = std::fs::read_to_string(&path)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if pid.is_empty() {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                match pid.parse::<u32>() {
                    Ok(pid) if process_alive(pid) => {
                        return Err(format!(
                            "这个 home 的 app-server 已经在跑（pid {pid}，锁文件 {}）。\
                             先停掉它，或用别的 JOY_HOME。",
                            path.display()
                        ));
                    }
                    _ => {
                        eprintln!(
                            "(joy) 清掉残留的守护锁（pid {pid} 已不存在）：{}",
                            path.display()
                        );
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            Err(e) => {
                return Err(format!("锁文件建不了（{}）：{e}", path.display()));
            }
        }
        let _ = attempt;
    }
    Err(format!(
        "锁文件反复建不出来（{}）—— 检查目录权限",
        path.display()
    ))
}

/// 进程还活着吗？Unix 用 `kill -0`（不存在则失败）；Windows 用 `tasklist` 过滤。
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(windows)]
    {
        let Ok(output) = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
    }
}
