//! 守护锁的测试：真冲突、崩溃残骸收尸、Drop 清理。
//!
//! 活进程判定用「起一个立即退出的子进程拿它的 pid」—— 那个 pid 必然死透，
//! 也不会撞上测试机上的真实长驻进程。

use super::lock;
use std::io::Write;

fn temp_home() -> tempfile::TempDir {
    tempfile::tempdir().expect("临时目录")
}

/// 第二个进程拿同一把锁：硬错误，错误里带着在跑的 PID。
#[test]
fn a_second_acquire_on_a_live_lock_fails() {
    let dir = temp_home();
    let first = lock::acquire(dir.path()).expect("第一次该拿到");
    let error = lock::acquire(dir.path()).unwrap_err();
    assert!(error.contains("已经在跑"), "{error}");
    assert!(
        error.contains(&std::process::id().to_string()),
        "该带 pid：{error}"
    );
    drop(first);
}

/// 锁文件在但进程死了：自动收尸，第二次能拿到。
#[test]
fn a_stale_lock_from_a_dead_process_is_reaped() {
    let dir = temp_home();
    // 造一个「必然已死」的 pid：起个立即退出的子进程，等它结束。
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

/// 锁文件内容损坏（不是数字）：同样当残骸收掉，不卡死人。
#[test]
fn a_corrupt_lock_file_is_also_reaped() {
    let dir = temp_home();
    let lock_path = dir.path().join("app-server.lock");
    std::fs::write(&lock_path, "不是数字").expect("造坏文件");
    let guard = lock::acquire(dir.path()).expect("坏文件该被收掉");
    drop(guard);
    assert!(!lock_path.exists());
}

/// 正常释放后可以再拿（REPL 关了再开的场景）。
#[test]
fn release_then_acquire_again() {
    let dir = temp_home();
    let first = lock::acquire(dir.path()).expect("第一次");
    drop(first);
    let _second = lock::acquire(dir.path()).expect("释放后第二次");
}
