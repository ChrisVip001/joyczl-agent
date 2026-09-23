//! 后台作业：起、读、列、停，以及 owner 围栏。
//!
//! 起真进程的用例要沙箱（`run_command` 那条纪律：没有沙箱就不执行任何命令），
//! 所以它们在没沙箱的机器上直接返回 —— 与 exec 的测试同一套跳过方式。
//! 环溢出那条是纯函数，任何平台都跑。

use std::sync::Arc;

use joyczl_tools::exec::ExecPolicy;
use joyczl_tools::jobs::JobRegistry;

use crate::jobs::{Jobs, Ring};

fn jobs(allow: &[&str]) -> (Arc<Jobs>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("临时目录");
    let policy = ExecPolicy {
        allow: allow.iter().map(|rule| rule.to_string()).collect(),
        timeout_secs: 10,
        network: false,
        approval: false,
        approval_timeout_secs: 5,
        spill_dir: None,
        extra_roots: Vec::new(),
    };
    (Arc::new(Jobs::new(policy, dir.path().to_path_buf())), dir)
}

/// 起一个作业、读一截、等它结束、结果落到 outbox。
#[tokio::test]
async fn a_job_runs_reads_and_lands_in_the_outbox() {
    if !joyczl_tools::exec::sandbox_available() {
        return;
    }
    let (jobs, dir) = jobs(&["echo*"]);

    let id = jobs
        .spawn("s1", "echo 第一行 && echo 第二行", None)
        .await
        .expect("起得来");

    // 等它结束（wait 有界，命令很短）。
    let read = jobs.output("s1", &id, 0, true).await.expect("读得到");
    assert!(!read.running, "echo 两条应该已经跑完：{read:?}");
    assert_eq!(read.exit_code, Some(0), "{read:?}");
    assert!(read.text.contains("第一行"), "{read:?}");
    assert!(read.text.contains("第二行"), "{read:?}");

    // cursor 接着上次：没有新东西了。
    let again = jobs
        .output("s1", &id, read.cursor, false)
        .await
        .expect("读得到");
    assert!(again.text.is_empty(), "接着读不该重复给：{again:?}");

    // 收尾落盘（进程退出后仍可回查）。
    let outbox = dir
        .path()
        .join("outbox")
        .join("jobs")
        .join(format!("{id}.txt"));
    assert!(outbox.exists(), "该落到 {}：{:?}", outbox.display(), again);
}

/// 别的会话拿到 id 也读不到、停不了。
#[tokio::test]
async fn another_session_cannot_touch_the_job() {
    if !joyczl_tools::exec::sandbox_available() {
        return;
    }
    let (jobs, _dir) = jobs(&["echo*"]);
    let id = jobs.spawn("mine", "echo 私有", None).await.expect("起得来");

    let stolen = jobs.output("someone-else", &id, 0, false).await;
    assert!(stolen.is_err(), "别人的作业不该读得到");
    let killed = jobs.kill("someone-else", &id).await;
    assert!(killed.is_err(), "别人的作业不该停得掉");
    assert!(jobs.list("someone-else").is_empty(), "列表里也不该有");
    assert_eq!(jobs.list("mine").len(), 1);
}

/// 停掉一个还在跑的：立刻算停，之后列表里显示已结束。
#[tokio::test]
async fn a_running_job_can_be_killed() {
    if !joyczl_tools::exec::sandbox_available() {
        return;
    }
    let (jobs, _dir) = jobs(&["echo*"]);
    let id = jobs
        .spawn("s1", "echo 起来 && sleep 30", None)
        .await
        .expect("起得来");

    // 先确认它真的还在跑（sleep 30 不会立刻结束）。
    let read = jobs.output("s1", &id, 0, false).await.expect("读得到");
    assert!(read.running, "{read:?}");

    assert!(jobs.kill("s1", &id).await.expect("停得掉"), "本来就还在跑");
    assert!(!jobs.kill("s1", &id).await.expect("再停一次"), "已经停了");
}

/// 环溢出：丢最旧的、记下丢了多少、读的时候如实说。
#[test]
fn an_overflowing_ring_drops_the_oldest_and_says_so() {
    let mut ring = Ring::new(8);
    ring.push(b"0123456789"); // 10 字节进 8 字节的环 → 丢前 2 个

    let (text, cursor, lossy) = ring.read_from(0);
    assert!(lossy, "从 0 读就是要丢东西的");
    assert_eq!(text, "23456789", "留最近的");
    assert_eq!(cursor, 10, "cursor 是绝对位置");
    assert_eq!(ring.total(), 10);

    // 从「现在」往后读：不丢东西，也不会重复给。
    let (text, cursor, lossy) = ring.read_from(cursor);
    assert!(text.is_empty() && !lossy);
    ring.push(b"ab");
    let (text, _, lossy) = ring.read_from(cursor);
    assert_eq!(text, "ab");
    assert!(!lossy, "接着读不该被算成丢失");
}

/// 一次写进去的比整个环还大：只留最后 cap 个字节。
#[test]
fn a_single_huge_write_keeps_only_the_tail() {
    let mut ring = Ring::new(4);
    ring.push(b"abcdefghij");
    let (text, cursor, lossy) = ring.read_from(0);
    assert_eq!(text, "ghij");
    assert!(lossy);
    assert_eq!(cursor, 10);
}
