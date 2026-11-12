//! Background jobs: start, read, list, stop, and the owner fence.
//!
//! The cases that start real processes need the sandbox (`run_command`'s discipline: no sandbox, no
//! command runs), so they return immediately on machines without one — the same skip style as exec's tests.
//! The ring-overflow case is a pure function and runs on any platform.

use std::sync::Arc;

use joyczl_tools::exec::ExecPolicy;
use joyczl_tools::jobs::JobRegistry;

use crate::jobs::{Jobs, Ring};

async fn jobs(allow: &[&str]) -> (Arc<Jobs>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("临时目录");
    let policy = ExecPolicy {
        allow: allow.iter().map(|rule| rule.to_string()).collect(),
        timeout_secs: 10,
        network: false,
        approval: false,
        approval_timeout_secs: 5,
        spill_dir: None,
        extra_roots: Vec::new(),
        home: std::path::PathBuf::new(),
        session_allow: Default::default(),
    };
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    (
        Arc::new(Jobs::new(
            policy,
            dir.path().to_path_buf(),
            joyczl_state::Jobs::new(pool),
        )),
        dir,
    )
}

/// Start a job, read a chunk, wait for it to finish, and land the result in the outbox.
#[tokio::test]
async fn a_job_runs_reads_and_lands_in_the_outbox() {
    if !joyczl_tools::exec::sandbox_available() {
        return;
    }
    let (jobs, dir) = jobs(&["echo*"]).await;

    let id = jobs
        .spawn("s1", "echo 第一行 && echo 第二行", None)
        .await
        .expect("起得来");

    // Wait for it to finish (wait is bounded; the command is short).
    let read = jobs.output("s1", &id, 0, true).await.expect("读得到");
    assert!(!read.running, "echo 两条应该已经跑完：{read:?}");
    assert_eq!(read.exit_code, Some(0), "{read:?}");
    assert!(read.text.contains("第一行"), "{read:?}");
    assert!(read.text.contains("第二行"), "{read:?}");

    // The cursor continues from last time: nothing new.
    let again = jobs
        .output("s1", &id, read.cursor, false)
        .await
        .expect("读得到");
    assert!(again.text.is_empty(), "接着读不该重复给：{again:?}");

    // Completion spill (still queryable after the process exits). The filename carries the **pid**:
    // job ids are per-process counters, and two Joy processes would overwrite each other's `j1.txt`.
    let outbox = dir
        .path()
        .join("outbox")
        .join("jobs")
        .join(format!("{}-{id}.txt", std::process::id()));
    assert!(outbox.exists(), "该落到 {}：{:?}", outbox.display(), again);
    let text = std::fs::read_to_string(&outbox).expect("读得回来");
    assert!(text.contains("退出码 0"), "{text}");
    assert!(text.contains("第一行"), "环里剩下的内容该在里面：{text}");
}

/// Another session cannot read or stop a job even with its id.
#[tokio::test]
async fn another_session_cannot_touch_the_job() {
    if !joyczl_tools::exec::sandbox_available() {
        return;
    }
    let (jobs, _dir) = jobs(&["echo*"]).await;
    let id = jobs.spawn("mine", "echo 私有", None).await.expect("起得来");

    let stolen = jobs.output("someone-else", &id, 0, false).await;
    assert!(stolen.is_err(), "别人的作业不该读得到");
    let killed = jobs.kill("someone-else", &id).await;
    assert!(killed.is_err(), "别人的作业不该停得掉");
    assert!(jobs.list("someone-else").is_empty(), "列表里也不该有");
    assert_eq!(jobs.list("mine").len(), 1);
}

/// Stop one that is still running: counted as stopped immediately, and the list shows it ended afterwards.
#[tokio::test]
async fn a_running_job_can_be_killed() {
    if !joyczl_tools::exec::sandbox_available() {
        return;
    }
    let (jobs, _dir) = jobs(&["echo*"]).await;
    let id = jobs
        .spawn("s1", "echo 起来 && sleep 30", None)
        .await
        .expect("起得来");

    // First confirm it really is still running (sleep 30 does not end right away).
    let read = jobs.output("s1", &id, 0, false).await.expect("读得到");
    assert!(read.running, "{read:?}");

    assert!(jobs.kill("s1", &id).await.expect("停得掉"), "本来就还在跑");
    assert!(!jobs.kill("s1", &id).await.expect("再停一次"), "已经停了");
}

/// Ring overflow: drop the oldest, record how much was dropped, and say so on read.
#[test]
fn an_overflowing_ring_drops_the_oldest_and_says_so() {
    let mut ring = Ring::new(8);
    ring.push(b"0123456789"); // 10 bytes into an 8-byte ring → the first 2 are dropped

    let (text, cursor, lossy) = ring.read_from(0);
    assert!(lossy, "从 0 读就是要丢东西的");
    assert_eq!(text, "23456789", "留最近的");
    assert_eq!(cursor, 10, "cursor 是绝对位置");
    assert_eq!(ring.total(), 10);

    // Reading forward from "now": nothing dropped, nothing repeated.
    let (text, cursor, lossy) = ring.read_from(cursor);
    assert!(text.is_empty() && !lossy);
    ring.push(b"ab");
    let (text, _, lossy) = ring.read_from(cursor);
    assert_eq!(text, "ab");
    assert!(!lossy, "接着读不该被算成丢失");
}

/// A single write larger than the whole ring: only the last cap bytes are kept.
#[test]
fn a_single_huge_write_keeps_only_the_tail() {
    let mut ring = Ring::new(4);
    ring.push(b"abcdefghij");
    let (text, cursor, lossy) = ring.read_from(0);
    assert_eq!(text, "ghij");
    assert!(lossy);
    assert_eq!(cursor, 10);
}

// ---- Always-on: ledger, crash reconciliation, history lookback ---------------------------------------

/// Start a real job; when it ends the ledger says completed; a killed one says killed.
#[tokio::test]
async fn the_ledger_records_how_jobs_end() {
    if !joyczl_tools::exec::sandbox_available() {
        return;
    }
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    let rows = joyczl_state::Jobs::new(pool);
    let mut policy = ExecPolicy {
        allow: vec!["echo*".to_string(), "sleep*".to_string()],
        timeout_secs: 10,
        network: false,
        approval: false,
        approval_timeout_secs: 5,
        spill_dir: None,
        extra_roots: Vec::new(),
        home: std::path::PathBuf::new(),
        session_allow: Default::default(),
    };
    policy.timeout_secs = 10;
    let jobs: Arc<Jobs> = Arc::new(Jobs::new(policy, dir.path().to_path_buf(), rows.clone()));

    // Finished run: completed.
    let id = jobs.spawn("s1", "echo 好", None).await.expect("起得来");
    let _ = jobs.output("s1", &id, 0, true).await.expect("读得到");
    let row = rows.get(&id).await.expect("查台账").expect("该有这一行");
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.exit_code, Some(0));
    let log = std::fs::read_to_string(row.log_path.expect("该有日志路径")).expect("日志读得回");
    assert!(log.contains("好"), "输出该随写随落：{log}");

    // Killed one: killed, not failed — "failed" and "stopped by someone" are two different things.
    let id2 = jobs.spawn("s1", "sleep 30", None).await.expect("起得来");
    assert!(jobs.kill("s1", &id2).await.expect("停得掉"));
    // Wait for the finishing task to write the ledger.
    for _ in 0..50 {
        if let Ok(Some(row)) = rows.get(&id2).await {
            if row.status == "killed" {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let row2 = rows.get(&id2).await.expect("查台账").expect("该有这一行");
    assert_eq!(row2.status, "killed", "{row2:?}");
}

/// Crash reconciliation: a running row left hanging when the last process died → interrupted; history
/// can be listed, output read back; **never auto-rerun**.
#[tokio::test]
async fn a_crash_leaves_an_interrupted_record_not_a_zombie() {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    let rows = joyczl_state::Jobs::new(pool);

    // Simulate the last process: started one running row, wrote half the output, then died.
    let log_path = dir.path().join("jobs").join("999-j1.log");
    std::fs::create_dir_all(log_path.parent().unwrap()).expect("建日志目录");
    std::fs::write(&log_path, "跑到一半的输出").expect("写半截输出");
    rows.record_started(
        "j1",
        "s1",
        "long-task --keep-going",
        &log_path.display().to_string(),
    )
    .await
    .expect("记台账");

    // New process starts: reconcile.
    let jobs = Jobs::new(
        ExecPolicy {
            allow: vec![],
            timeout_secs: 10,
            network: false,
            approval: false,
            approval_timeout_secs: 5,
            spill_dir: None,
            extra_roots: Vec::new(),
            home: std::path::PathBuf::new(),
            session_allow: Default::default(),
        },
        dir.path().to_path_buf(),
        rows.clone(),
    );
    jobs.reconcile().await;

    let row = rows.get("j1").await.expect("查台账").expect("还在");
    assert_eq!(row.status, "interrupted", "挂着的就是中断了：{row:?}");
    assert!(row.finished_at.is_some(), "补上收尾时间：{row:?}");

    // The history list is visible (owner fence as usual).
    assert_eq!(jobs.list("s1").len(), 1, "owner 该看得见");
    assert!(jobs.list("someone-else").is_empty(), "别人的历史不该看得见");

    // Output read-back: the half-written output is still there; it cannot run again but it can be read.
    let read = jobs
        .output("s1", &"j1".to_string(), 0, false)
        .await
        .expect("历史作业该读得到");
    assert!(!read.running);
    assert!(read.text.contains("跑到一半"), "{read:?}");
    // Others cannot read it — same wording as for live jobs.
    assert!(jobs
        .output("someone-else", &"j1".to_string(), 0, false)
        .await
        .is_err());
}
