//! 定时任务的匹配与装载：cron 语义、同一分钟只触发一次、两条声明来源。

use std::collections::HashMap;

use chrono::NaiveDate;

use super::{cron_matches, due_now, last_missed, load_jobs, load_legacy_state, Delivery, Job};

fn at(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> chrono::NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d)
        .expect("合法日期")
        .and_hms_opt(hh, mm, 0)
        .expect("合法时间")
}

fn job(name: &str, cron: &str) -> Job {
    Job {
        name: name.to_string(),
        cron: cron.to_string(),
        prompt: "做点事".to_string(),
        catch_up: true,
        delivery: Delivery::Outbox,
    }
}

#[test]
fn every_minute_matches_everything() {
    assert!(cron_matches("* * * * *", at(2026, 9, 21, 3, 7)));
}

#[test]
fn a_daily_time_matches_only_that_minute() {
    // 2026-09-21 是周一。
    assert!(cron_matches("0 8 * * *", at(2026, 9, 21, 8, 0)));
    assert!(!cron_matches("0 8 * * *", at(2026, 9, 21, 8, 1)));
    assert!(!cron_matches("0 8 * * *", at(2026, 9, 21, 9, 0)));
}

#[test]
fn steps_ranges_and_lists_all_work() {
    // 每 15 分钟。
    assert!(cron_matches("*/15 * * * *", at(2026, 9, 21, 0, 30)));
    assert!(!cron_matches("*/15 * * * *", at(2026, 9, 21, 0, 31)));
    // 工作日 9 点到 17 点整点。
    assert!(cron_matches("0 9-17 * * 1-5", at(2026, 9, 21, 9, 0)));
    assert!(cron_matches("0 9-17 * * 1-5", at(2026, 9, 25, 17, 0)));
    assert!(
        !cron_matches("0 9-17 * * 1-5", at(2026, 9, 26, 9, 0)),
        "周六不该触发"
    );
    // 列表，含周日的 7 形式。
    assert!(cron_matches("0 8,20 * * *", at(2026, 9, 21, 20, 0)));
    assert!(!cron_matches("0 8,20 * * *", at(2026, 9, 21, 12, 0)));
    assert!(cron_matches("0 0 * * 7", at(2026, 9, 27, 0, 0)), "周日");
}

#[test]
fn a_malformed_cron_never_fires() {
    assert!(!cron_matches("0 8 * *", at(2026, 9, 21, 8, 0)), "四字段");
    assert!(!cron_matches("banana", at(2026, 9, 21, 8, 0)));
    assert!(!cron_matches("0 8 * * 1-", at(2026, 9, 21, 8, 0)));
}

#[test]
fn the_same_minute_only_fires_once() {
    let jobs = vec![job("brief", "0 8 * * *")];
    let fired = HashMap::new();
    let first = due_now(&jobs, at(2026, 9, 21, 8, 0), &fired);
    assert_eq!(first.len(), 1);

    let mut fired = HashMap::new();
    fired.insert("brief".to_string(), "2026-09-21T08:00".to_string());
    let second = due_now(&jobs, at(2026, 9, 21, 8, 0), &fired);
    assert!(second.is_empty(), "同一分钟不该触发第二次");
    // 下一分钟（含 8:00 的每日任务）自然也不触发。
    let later = due_now(&jobs, at(2026, 9, 21, 8, 1), &fired);
    assert!(later.is_empty());
}

/// 真触发一次：进程内起 Server、装上 scripted 模型、跑一轮，断言回复
/// 落进了 outbox —— 定时任务的最后一公里（run_turn → 文件）值得钉住。
#[tokio::test]
async fn firing_runs_a_turn_and_writes_the_reply_to_the_outbox() {
    use std::sync::Arc;

    use joyczl_provider::mock::Mock;
    use joyczl_provider::Resolved;

    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let settings = joyczl_config::Settings {
        home: home.clone(),
        api_key: Some("dummy".to_string()),
        ..Default::default()
    };
    let server = joyczl_app_server::open(&settings).await.expect("装配");
    server.install_provider(Resolved {
        provider_id: "mock".to_string(),
        client: Arc::new(Mock::new(vec![
            // 检索门，然后 loop 的应答。
            Mock::text(r#"{"retrieve": false, "query": "", "reason": "routine"}"#),
            Mock::text("打卡完成。"),
        ])),
        model: "test".to_string(),
        small_model: "test-small".to_string(),
    });

    let job = job("tick", "* * * * *");
    super::fire(&server, &job, at(2026, 9, 21, 9, 0))
        .await
        .expect("触发成功");

    let path = home.join("outbox").join("schedule-tick-20260921-0900.md");
    let written = std::fs::read_to_string(&path).expect("结果文件");
    assert!(written.contains("打卡完成。"), "{written}");
    assert!(
        written.contains("cron '* * * * *'"),
        "要写明触发来源：{written}"
    );

    // 定时会话与用户会话分开：这次执行的历史落在 schedule:tick 里。
    let sessions = server.chat().sessions().await.expect("会话列表");
    assert!(
        sessions.iter().any(|s| s.id == "schedule:tick"),
        "定时任务该有自己的会话：{sessions:?}"
    );
}

#[test]
fn jobs_load_from_both_skills_and_the_json_file() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path();
    // 一条带 schedule 的技能。
    let skill_dir = home.join("skills").join("weekly-review");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: weekly-review\ndescription: summarize the week\nschedule: 0 8 * * 1\n---\n步骤",
    )
    .unwrap();
    // 一条不带 schedule 的技能：不该变成任务。
    let plain = home.join("skills").join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(
        plain.join("SKILL.md"),
        "---\nname: plain\ndescription: nothing scheduled\n---\n步骤",
    )
    .unwrap();
    // schedules.json 里两条（一条坏的）。
    std::fs::write(
        home.join("schedules.json"),
        r#"{"jobs":[
            {"name":"standup","cron":"0 9 * * 1-5","prompt":"今天要做什么？"},
            {"name":"broken","cron":"0 9 * * 1-5"}
        ]}"#,
    )
    .unwrap();

    let jobs = load_jobs(home);
    let names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
    assert_eq!(names, vec!["weekly-review", "standup"]);
    let weekly = jobs.iter().find(|j| j.name == "weekly-review").unwrap();
    assert_eq!(weekly.cron, "0 8 * * 1");
    assert!(
        weekly.prompt.contains("summarize the week"),
        "技能任务的请求要带上它自己的描述：{}",
        weekly.prompt
    );
}

// ---- 7x24：状态落盘与补跑窗口 ---------------------------------------------

/// 旧状态文件读得回来（一次性导入的来源）；坏文件当没有 ——
/// 它只承载去重信息，丢了顶多同分钟重复一次，不该让调度器起不来。
#[test]
fn the_legacy_state_file_is_readable_but_only_once() {
    let dir = tempfile::tempdir().expect("临时目录");
    let mut fired = HashMap::new();
    fired.insert("晨报".to_string(), "2026-10-27T08:00".to_string());
    let text = serde_json::to_string(&fired).expect("序列化");
    std::fs::write(dir.path().join("schedules-state.json"), text).expect("写文件");

    let loaded = load_legacy_state(dir.path());
    assert_eq!(
        loaded.get("晨报").map(String::as_str),
        Some("2026-10-27T08:00")
    );

    // 坏文件当没有。
    std::fs::write(dir.path().join("schedules-state.json"), "{半截").expect("写坏文件");
    assert!(load_legacy_state(dir.path()).is_empty());
}

/// schedules.json 的每作业策略字段：catch_up 与 delivery 都解析得动，
/// 不认识的 delivery 按 outbox 兜底（不静默 —— stderr 说一声）。
#[test]
fn per_job_policy_fields_are_parsed() {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(
        dir.path().join("schedules.json"),
        r#"{"jobs": [
            {"name": "晨报", "cron": "0 8 * * *", "prompt": "早", "catch_up": false, "delivery": "notify"},
            {"name": "盯CI", "cron": "*/15 * * * *", "prompt": "看看", "delivery": "quiet"},
            {"name": "默认", "cron": "0 12 * * *", "prompt": "午"},
            {"name": "写错", "cron": "0 13 * * *", "prompt": "午", "delivery": "每日一报"}
        ]}"#,
    )
    .expect("写声明");
    let jobs = load_jobs(dir.path());
    assert_eq!(jobs.len(), 4);
    let morning = jobs.iter().find(|j| j.name == "晨报").expect("晨报");
    assert!(!morning.catch_up, "catch_up=false 至多一次");
    assert_eq!(morning.delivery, Delivery::Notify);
    let ci = jobs.iter().find(|j| j.name == "盯CI").expect("盯CI");
    assert_eq!(ci.delivery, Delivery::Quiet);
    let plain = jobs.iter().find(|j| j.name == "默认").expect("默认");
    assert!(plain.catch_up, "默认至少一次");
    assert_eq!(plain.delivery, Delivery::Outbox);
    let wrong = jobs.iter().find(|j| j.name == "写错").expect("写错");
    assert_eq!(wrong.delivery, Delivery::Outbox, "不认识的值兜底");
}

/// 停机窗口里错过了一次 → 补最新那次；没错过 → 不补；三天前错过只补最新的。
#[test]
fn a_missed_fire_is_caught_up_once() {
    let daily = job("晨报", "0 8 * * *");

    // 停机窗口 7:00-9:00 覆盖了 8:00 → 补 8:00。
    let missed = last_missed(&daily, at(2026, 10, 27, 7, 0), at(2026, 10, 27, 9, 0));
    assert_eq!(missed, Some(at(2026, 10, 27, 8, 0)));

    // 窗口里没有触发点 → 不补。
    assert_eq!(
        last_missed(&daily, at(2026, 10, 27, 9, 0), at(2026, 10, 27, 9, 30)),
        None
    );

    // 起点本身算错过（上次触发 7:59，8:00 的点属于「错过」而非「已触发」）。
    let missed = last_missed(&daily, at(2026, 10, 26, 7, 59), at(2026, 10, 27, 8, 30));
    assert_eq!(
        missed,
        Some(at(2026, 10, 27, 8, 0)),
        "三天停机也只补最新一次"
    );
}

/// 每分钟任务在长停机窗口里也只交出最后一个错过的分钟。
#[test]
fn an_every_minute_job_yields_only_the_latest_missed_minute() {
    let frequent = job("心跳", "* * * * *");
    let missed = last_missed(&frequent, at(2026, 10, 20, 0, 0), at(2026, 10, 27, 0, 0));
    assert_eq!(missed, Some(at(2026, 10, 27, 0, 0)));
}
