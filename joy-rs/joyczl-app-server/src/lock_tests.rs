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
    let first = lock::acquire(dir.path()).expect("the first one should get it");
    let error = lock::acquire(dir.path()).unwrap_err();
    assert!(error.contains("already running"), "{error}");
    assert!(
        error.contains(&std::process::id().to_string()),
        "it has to carry the pid: {error}"
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
        .expect("start the subprocess");
    let dead_pid = child.id();
    child.wait().expect("wait for the child to exit");

    let lock_path = dir.path().join("app-server.lock");
    let mut file = std::fs::File::create(&lock_path).expect("plant the wreckage");
    writeln!(file, "{dead_pid}").expect("write the pid");

    let second = lock::acquire(dir.path()).expect("the wreckage should be collected");
    drop(second);
    assert!(
        !lock_path.exists(),
        "after Drop the lock file should be gone"
    );
}

/// A corrupt lock file (not a number): reaped like wreckage too, never a hang.
#[test]
fn a_corrupt_lock_file_is_also_reaped() {
    let dir = temp_home();
    let lock_path = dir.path().join("app-server.lock");
    std::fs::write(&lock_path, "not a number").expect("plant the broken file");
    let guard = lock::acquire(dir.path()).expect("a bad file should be collected");
    drop(guard);
    assert!(!lock_path.exists());
}

/// After a normal release it can be acquired again (the REPL closed and reopened).
#[test]
fn release_then_acquire_again() {
    let dir = temp_home();
    let first = lock::acquire(dir.path()).expect("the first time");
    drop(first);
    let _second = lock::acquire(dir.path()).expect("the second one after the release");
}

/// A separate test process holds the OS lock until the parent kills it.
#[test]
fn child_process_ownership_probe() {
    let Some(home) = std::env::var_os("JOY_TEST_LOCK_PROBE_HOME") else {
        return;
    };
    let home = std::path::PathBuf::from(home);
    let _owner = lock::acquire(&home).expect("child acquires execution ownership");
    std::fs::write(home.join("ownership.ready"), "ready").unwrap();
    std::thread::sleep(std::time::Duration::from_secs(10));
}

#[test]
fn a_crashed_process_releases_kernel_ownership_without_deleting_its_inode() {
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let home = temp_home();
    let mut child = ChildGuard(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "lock_tests::child_process_ownership_probe",
                "--nocapture",
            ])
            .env("JOY_TEST_LOCK_PROBE_HOME", home.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !home.path().join("ownership.ready").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "Child did not publish ownership readiness"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(lock::acquire(home.path()).is_err());
    let inode = home.path().join("state.db.runtime.lock");
    assert!(inode.exists());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let next = lock::acquire(home.path()).unwrap();
    drop(next);
    assert!(
        inode.exists(),
        "The advisory lock inode must remain stable after release"
    );
}
