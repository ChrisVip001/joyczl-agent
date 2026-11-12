//! Guardian lock tests: a real conflict, reaping crash wreckage, Drop cleanup.
//!
//! Liveness is judged by "spawn a child that exits immediately and take its pid" — that pid is certainly
//! dead, and will not collide with a real long-lived process on the test machine.

use super::lock;
use std::io::Write;

fn temp_home() -> tempfile::TempDir {
    tempfile::tempdir().expect("temporary directory")
}

/// A second process takes the same lock: a hard error, carrying the running PID.
#[test]
fn a_second_acquire_on_a_live_lock_fails() {
    let dir = temp_home();
    let first = lock::acquire(dir.path()).expect("第一次该拿到");
    let error = lock::acquire(dir.path()).unwrap_err();
    assert!(error.contains("already running"), "{error}");
    assert!(
        error.contains(&std::process::id().to_string()),
        "该带 pid：{error}"
    );
    drop(first);
}

/// The lock file exists but the process is dead: reaped automatically, the second acquire succeeds.
#[test]
fn a_stale_lock_from_a_dead_process_is_reaped() {
    let dir = temp_home();
    // Manufacture a "certainly dead" pid: spawn a child that exits immediately and wait for it.
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("起子进程");
    let dead_pid = child.id();
    child.wait().expect("等子进程退出");

    let lock_path = dir.path().join("app-server.lock");
    let mut file = std::fs::File::create(&lock_path).expect("造残骸");
    writeln!(file, "{dead_pid}").expect("写 pid");

    let second = lock::acquire(dir.path()).expect("残骸该被收掉");
    drop(second);
    assert!(!lock_path.exists(), "Drop 后锁文件该没了");
}

/// A corrupt lock file (not a number): reaped like wreckage too, never a hang.
#[test]
fn a_corrupt_lock_file_is_also_reaped() {
    let dir = temp_home();
    let lock_path = dir.path().join("app-server.lock");
    std::fs::write(&lock_path, "不是数字").expect("造坏文件");
    let guard = lock::acquire(dir.path()).expect("坏文件该被收掉");
    drop(guard);
    assert!(!lock_path.exists());
}

/// After a normal release it can be acquired again (the REPL closed and reopened).
#[test]
fn release_then_acquire_again() {
    let dir = temp_home();
    let first = lock::acquire(dir.path()).expect("第一次");
    drop(first);
    let _second = lock::acquire(dir.path()).expect("释放后第二次");
}
