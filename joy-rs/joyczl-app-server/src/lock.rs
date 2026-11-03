//! Guardian lock: only one app-server process per `<home>`.
//!
//! A PID file plus an atomic create: the PID file guards against **real accidents** — two app-servers
//! each holding their own in-memory state (approvals, goals, job table), writing the same state.db,
//! neither knowing the other exists. "Already running" must be a hard startup error, not a warning.
//!
//! Three semantics:
//! * Acquiring the lock = `O_EXCL`-create the file and write our own PID;
//! * File exists → read the PID and check whether the process is alive: alive means error out; dead means
//!   the wreckage of the last crash, **clear it and retry** (no manual file deletion for humans);
//! * `Drop` deletes the file. A crash failing to clean up is fine — the next process reaps by the rules above.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Lock file name (placed under `<home>`, same convention as the other state files).
const LOCK_FILE: &str = "app-server.lock";

/// The guardian lock. The lock file exists while held; `Drop` deletes it.
#[derive(Debug)]
pub struct DaemonLock {
    path: PathBuf,
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Try to acquire the guardian lock for `home`. On failure returns an error with the reason (including the running PID and file path).
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
                // Lock already held: a live process is a real conflict; a dead one is crash wreckage — reap and retry.
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

/// Is the process alive? Unix uses `kill -0` (fails if absent); Windows filters `tasklist`.
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
