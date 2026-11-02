//! 后台作业：起、读、列、停，以及 owner 围栏。
//!
//! 起真进程的用例要沙箱（`run_command` 那条纪律：没有沙箱就不执行任何命令），
//! 所以它们在没沙箱的机器上直接返回 —— 与 exec 的测试同一套跳过方式。
//! 环溢出那条是纯函数，任何平台都跑。

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

/// 起一个作业、读一截、等它结束、结果落到 outbox。
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

    // 收尾落盘（进程退出后仍可回查）。文件名带 **pid**：job id 是进程内自增的，
    // 两个 Joy 进程会生成同名 `j1.txt` 互相覆盖。
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

/// 别的会话拿到 id 也读不到、停不了。
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

/// 停掉一个还在跑的：立刻算停，之后列表里显示已结束。
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

// ---- 7x24：台账、崩溃对账、历史回查 ---------------------------------------

/// 起一个真 job，结束后台账里是 completed；被 kill 的是 killed。
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
    };
    policy.timeout_secs = 10;
    let jobs: Arc<Jobs> = Arc::new(Jobs::new(policy, dir.path().to_path_buf(), rows.clone()));

    // 跑完的：completed。
    let id = jobs.spawn("s1", "echo 好", None).await.expect("起得来");
    let _ = jobs.output("s1", &id, 0, true).await.expect("读得到");
    let row = rows.get(&id).await.expect("查台账").expect("该有这一行");
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.exit_code, Some(0));
    let log = std::fs::read_to_string(row.log_path.expect("该有日志路径")).expect("日志读得回");
    assert!(log.contains("好"), "输出该随写随落：{log}");

    // 被停的：killed，不是 failed —— 「失败」和「被人叫停」是两回事。
    let id2 = jobs.spawn("s1", "sleep 30", None).await.expect("起得来");
    assert!(jobs.kill("s1", &id2).await.expect("停得掉"));
    // 等收尾 task 把台账写上。
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

/// 崩溃对账：上次进程死时还挂着的 running 行 → interrupted；历史能列出、
/// 输出能回读；**不自动重跑**。
#[tokio::test]
async fn a_crash_leaves_an_interrupted_record_not_a_zombie() {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    let rows = joyczl_state::Jobs::new(pool);

    // 模拟上一次进程：起了一行 running，输出写了一半，然后死了。
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

    // 新进程启动：对账。
    let jobs = Jobs::new(
        ExecPolicy {
            allow: vec![],
            timeout_secs: 10,
            network: false,
            approval: false,
            approval_timeout_secs: 5,
            spill_dir: None,
            extra_roots: Vec::new(),
        },
        dir.path().to_path_buf(),
        rows.clone(),
    );
    jobs.reconcile().await;

    let row = rows.get("j1").await.expect("查台账").expect("还在");
    assert_eq!(row.status, "interrupted", "挂着的就是中断了：{row:?}");
    assert!(row.finished_at.is_some(), "补上收尾时间：{row:?}");

    // 历史列表看得见（owner 围栏照旧）。
    assert_eq!(jobs.list("s1").len(), 1, "owner 该看得见");
    assert!(jobs.list("someone-else").is_empty(), "别人的历史不该看得见");

    // 输出回读：半截输出还在，跑不回去但读得回来。
    let read = jobs
        .output("s1", &"j1".to_string(), 0, false)
        .await
        .expect("历史作业该读得到");
    assert!(!read.running);
    assert!(read.text.contains("跑到一半"), "{read:?}");
    // 别人读不到 —— 消息与活作业同一个口径。
    assert!(jobs
        .output("someone-else", &"j1".to_string(), 0, false)
        .await
        .is_err());
}
