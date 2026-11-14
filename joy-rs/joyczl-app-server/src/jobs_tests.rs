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
    let dir = tempfile::tempdir().expect("temporary directory");
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
        .expect("open the store");
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
        .spawn("s1", "echo the first line && echo the second line", None)
        .await
        .expect("it starts");

    // Wait for it to finish (wait is bounded; the command is short).
    let read = jobs.output("s1", &id, 0, true).await.expect("it reads");
    assert!(
        !read.running,
        "the two echoes should have finished: {read:?}"
    );
    assert_eq!(read.exit_code, Some(0), "{read:?}");
    assert!(read.text.contains("the first line"), "{read:?}");
    assert!(read.text.contains("the second line"), "{read:?}");

    // The cursor continues from last time: nothing new.
    let again = jobs
        .output("s1", &id, read.cursor, false)
        .await
        .expect("it reads");
    assert!(
        again.text.is_empty(),
        "reading on must not repeat anything: {again:?}"
    );

    // Completion spill (still queryable after the process exits). The filename carries the **pid**:
    // job ids are per-process counters, and two Joy processes would overwrite each other's `j1.txt`.
    let outbox = dir
        .path()
        .join("outbox")
        .join("jobs")
        .join(format!("{}-{id}.txt", std::process::id()));
    assert!(
        outbox.exists(),
        "it should land in {}: {:?}",
        outbox.display(),
        again
    );
    let text = std::fs::read_to_string(&outbox).expect("it reads back");
    assert!(text.contains("exit code 0"), "{text}");
    assert!(
        text.contains("the first line"),
        "what the ring still held should be in there: {text}"
    );
}

/// Another session cannot read or stop a job even with its id.
#[tokio::test]
async fn another_session_cannot_touch_the_job() {
    if !joyczl_tools::exec::sandbox_available() {
        return;
    }
    let (jobs, _dir) = jobs(&["echo*"]).await;
    let id = jobs
        .spawn("mine", "echo private", None)
        .await
        .expect("it starts");

    let stolen = jobs.output("someone-else", &id, 0, false).await;
    assert!(
        stolen.is_err(),
        "another session's job must not be readable"
    );
    let killed = jobs.kill("someone-else", &id).await;
    assert!(
        killed.is_err(),
        "another session's job must not be killable"
    );
    assert!(
        jobs.list("someone-else").is_empty(),
        "nor should it show up in a list"
    );
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
        .spawn("s1", "echo up && sleep 30", None)
        .await
        .expect("it starts");

    // First confirm it really is still running (sleep 30 does not end right away).
    let read = jobs.output("s1", &id, 0, false).await.expect("it reads");
    assert!(read.running, "{read:?}");

    assert!(
        jobs.kill("s1", &id).await.expect("it stops"),
        "it was still running"
    );
    assert!(
        !jobs.kill("s1", &id).await.expect("stop it again"),
        "it had already stopped"
    );
}

/// Ring overflow: drop the oldest, record how much was dropped, and say so on read.
#[test]
fn an_overflowing_ring_drops_the_oldest_and_says_so() {
    let mut ring = Ring::new(8);
    ring.push(b"0123456789"); // 10 bytes into an 8-byte ring → the first 2 are dropped

    let (text, cursor, lossy) = ring.read_from(0);
    assert!(lossy, "reading from 0 has to lose something");
    assert_eq!(text, "23456789", "the most recent stays");
    assert_eq!(cursor, 10, "the cursor is an absolute position");
    assert_eq!(ring.total(), 10);

    // Reading forward from "now": nothing dropped, nothing repeated.
    let (text, cursor, lossy) = ring.read_from(cursor);
    assert!(text.is_empty() && !lossy);
    ring.push(b"ab");
    let (text, _, lossy) = ring.read_from(cursor);
    assert_eq!(text, "ab");
    assert!(!lossy, "reading on must not count as loss");
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
    let dir = tempfile::tempdir().expect("temporary directory");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("open the store");
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
    let id = jobs
        .spawn("s1", "echo fine", None)
        .await
        .expect("it starts");
    let _ = jobs.output("s1", &id, 0, true).await.expect("it reads");
    let row = rows
        .get(&id)
        .await
        .expect("query the register")
        .expect("the row should be there");
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.exit_code, Some(0));
    let log = std::fs::read_to_string(row.log_path.expect("it has a log path"))
        .expect("the log reads back");
    assert!(
        log.contains("fine"),
        "output should land as it is written: {log}"
    );

    // Killed one: killed, not failed — "failed" and "stopped by someone" are two different things.
    let id2 = jobs.spawn("s1", "sleep 30", None).await.expect("it starts");
    assert!(jobs.kill("s1", &id2).await.expect("it stops"));
    // Wait for the finishing task to write the ledger.
    for _ in 0..50 {
        if let Ok(Some(row)) = rows.get(&id2).await {
            if row.status == "killed" {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let row2 = rows
        .get(&id2)
        .await
        .expect("query the register")
        .expect("this row should be here");
    assert_eq!(row2.status, "killed", "{row2:?}");
}

/// Crash reconciliation: a running row left hanging when the last process died → interrupted; history
/// can be listed, output read back; **never auto-rerun**.
#[tokio::test]
async fn a_crash_leaves_an_interrupted_record_not_a_zombie() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("open the store");
    let rows = joyczl_state::Jobs::new(pool);

    // Simulate the last process: started one running row, wrote half the output, then died.
    let log_path = dir.path().join("jobs").join("999-j1.log");
    std::fs::create_dir_all(log_path.parent().unwrap()).expect("create the log directory");
    std::fs::write(&log_path, "output from halfway through").expect("write half the output");
    rows.record_started(
        "j1",
        "s1",
        "long-task --keep-going",
        &log_path.display().to_string(),
    )
    .await
    .expect("record it in the register");

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

    let row = rows
        .get("j1")
        .await
        .expect("query the register")
        .expect("still there");
    assert_eq!(
        row.status, "interrupted",
        "one left hanging is interrupted: {row:?}"
    );
    assert!(
        row.finished_at.is_some(),
        "the finish time is filled in: {row:?}"
    );

    // The history list is visible (owner fence as usual).
    assert_eq!(jobs.list("s1").len(), 1, "the owner should see it");
    assert!(
        jobs.list("someone-else").is_empty(),
        "another session's history must stay invisible"
    );

    // Output read-back: the half-written output is still there; it cannot run again but it can be read.
    let read = jobs
        .output("s1", &"j1".to_string(), 0, false)
        .await
        .expect("a finished job should still read");
    assert!(!read.running);
    assert!(read.text.contains("halfway through"), "{read:?}");
    // Others cannot read it — same wording as for live jobs.
    assert!(jobs
        .output("someone-else", &"j1".to_string(), 0, false)
        .await
        .is_err());
}

#[tokio::test]
async fn job_ids_survive_registry_recreation() {
    if !joyczl_tools::exec::sandbox_available() {
        return;
    }
    let (first, dir) = jobs(&["echo*"]).await;
    let id = first.spawn("s1", "echo first", None).await.unwrap();
    first.output("s1", &id, 0, true).await.unwrap();
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .unwrap();
    let rows = joyczl_state::Jobs::new(pool);
    let second = Jobs::new(
        ExecPolicy::default(),
        dir.path().to_path_buf(),
        rows.clone(),
    );
    let second_id = second.spawn("s1", "echo second", None).await.unwrap();
    second.output("s1", &second_id, 0, true).await.unwrap();
    assert_ne!(id, second_id);
    assert_eq!(rows.get(&id).await.unwrap().unwrap().command, "echo first");
    assert_eq!(
        rows.get(&second_id).await.unwrap().unwrap().command,
        "echo second"
    );
}

#[tokio::test]
async fn failed_job_ledger_prevents_process_start() {
    let (registry, dir) = jobs(&[]).await;
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .unwrap();
    sqlx::query("DROP TABLE jobs").execute(&pool).await.unwrap();
    let result = registry
        .spawn(
            "s1",
            "touch should-not-exist",
            Some(dir.path().display().to_string()),
        )
        .await;
    assert!(result.unwrap_err().contains("Cannot persist job intent"));
    assert!(!dir.path().join("should-not-exist").exists());
}

#[tokio::test]
async fn unavailable_task_run_prevents_background_process_start() {
    let (jobs, home) = jobs(&["echo*"]).await;
    let marker = home.path().join("must-not-run");
    let command = format!("echo started > {}", marker.display());
    assert!(jobs
        .spawn_for_run("s", &command, None, Some("missing-run".into()))
        .await
        .is_err());
    assert!(!marker.exists());
    assert!(jobs.list("s").is_empty());
}
