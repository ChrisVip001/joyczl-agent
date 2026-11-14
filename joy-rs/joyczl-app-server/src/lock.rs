//! Kernel-backed execution ownership shared by every checked runtime entry point.
//!
//! The advisory lock follows the canonical database path and stays on a persistent inode.
//! Its file is never unlinked; the OS releases ownership when the last guard closes or crashes.
//! The legacy PID file remains for diagnostics and interoperability with older app-server binaries.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Lock file name (placed under `<home>`, same convention as the other state files).
const LOCK_FILE: &str = "app-server.lock";

/// The guardian lock. The lock file exists while held; `Drop` deletes it.
#[derive(Debug)]
pub struct DaemonLock {
    path: PathBuf,
    _ownership: std::fs::File,
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Err(error) = fs2::FileExt::unlock(&self._ownership) {
            eprintln!("(joy) runtime ownership unlock failed: {error}");
        }
    }
}

/// Try to acquire the guardian lock for `home`. On failure returns an error with the reason (including the running PID and file path).
pub fn acquire(home: &Path) -> Result<DaemonLock, String> {
    let database = home.join("state.db");
    let resolved = match std::fs::canonicalize(&database) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(&database).is_ok_and(|meta| meta.file_type().is_symlink())
            {
                return Err(
                    "State database symlink target is unavailable; ownership cannot be established"
                        .into(),
                );
            }
            std::fs::canonicalize(home)
                .map_err(|error| format!("Cannot resolve runtime home: {error}"))?
                .join("state.db")
        }
        Err(error) => return Err(format!("Cannot resolve state database ownership: {error}")),
    };
    let mut name = resolved
        .file_name()
        .ok_or("State database has no filename")?
        .to_os_string();
    name.push(".runtime.lock");
    let lock_path = resolved
        .parent()
        .ok_or("State database has no directory")?
        .join(name);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let ownership = options.open(&lock_path).map_err(|error| {
        format!(
            "Cannot open runtime ownership lock ({}): {error}",
            lock_path.display()
        )
    })?;
    if let Err(error) = fs2::FileExt::try_lock_exclusive(&ownership) {
        let contended = fs2::lock_contended_error();
        if error.kind() == std::io::ErrorKind::WouldBlock
            || error
                .raw_os_error()
                .is_some_and(|code| Some(code) == contended.raw_os_error())
        {
            let pid = std::fs::read_to_string(home.join(LOCK_FILE))
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "unknown".into());
            return Err(format!("A Joy runtime for this state database is already running (pid {pid}, lock {}). Use its client or stop it before starting another runtime.",lock_path.display()));
        }
        return Err(format!(
            "Runtime ownership lock failed ({}): {error}",
            lock_path.display()
        ));
    }
    acquire_legacy(home, ownership)
}

fn acquire_legacy(home: &Path, ownership: std::fs::File) -> Result<DaemonLock, String> {
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
                    format!(
                        "the lock file could not be written ({}): {e}",
                        path.display()
                    )
                })?;
                return Ok(DaemonLock {
                    path,
                    _ownership: ownership,
                });
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
                            "An app-server for this home is already running (pid {pid}, lock file {}). Stop it first, or use a different JOY_HOME.",
                            path.display()
                        ));
                    }
                    _ => {
                        eprintln!(
                            "(joy) cleared a stale daemon lock (pid {pid} no longer exists): {}",
                            path.display()
                        );
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            Err(e) => {
                return Err(format!(
                    "the lock file could not be created ({}): {e}",
                    path.display()
                ));
            }
        }
        let _ = attempt;
    }
    Err(format!(
        "the lock file could not be created after several attempts ({}) — check the directory permissions",
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
