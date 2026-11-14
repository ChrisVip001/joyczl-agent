//! Matching and loading of scheduled jobs: cron semantics, once per minute, two sources.

use std::collections::HashMap;

use chrono::NaiveDate;

use super::{cron_matches, due_now, last_missed, load_jobs, load_legacy_state, Delivery, Job};

fn at(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> chrono::NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d)
        .expect("a legal date")
        .and_hms_opt(hh, mm, 0)
        .expect("a legal time")
}

fn job(name: &str, cron: &str) -> Job {
    Job {
        name: name.to_string(),
        cron: cron.to_string(),
        prompt: "do something".to_string(),
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
    // 2026-09-21 is a Monday.
    assert!(cron_matches("0 8 * * *", at(2026, 9, 21, 8, 0)));
    assert!(!cron_matches("0 8 * * *", at(2026, 9, 21, 8, 1)));
    assert!(!cron_matches("0 8 * * *", at(2026, 9, 21, 9, 0)));
}

#[test]
fn steps_ranges_and_lists_all_work() {
    // Every 15 minutes.
    assert!(cron_matches("*/15 * * * *", at(2026, 9, 21, 0, 30)));
    assert!(!cron_matches("*/15 * * * *", at(2026, 9, 21, 0, 31)));
    // Weekdays, on the hour 9:00-17:00.
    assert!(cron_matches("0 9-17 * * 1-5", at(2026, 9, 21, 9, 0)));
    assert!(cron_matches("0 9-17 * * 1-5", at(2026, 9, 25, 17, 0)));
    assert!(
        !cron_matches("0 9-17 * * 1-5", at(2026, 9, 26, 9, 0)),
        "Saturday should not fire"
    );
    // List form, including Sunday-as-7.
    assert!(cron_matches("0 8,20 * * *", at(2026, 9, 21, 20, 0)));
    assert!(!cron_matches("0 8,20 * * *", at(2026, 9, 21, 12, 0)));
    assert!(cron_matches("0 0 * * 7", at(2026, 9, 27, 0, 0)), "Sunday");
}

#[test]
fn a_malformed_cron_never_fires() {
    assert!(
        !cron_matches("0 8 * *", at(2026, 9, 21, 8, 0)),
        "four fields"
    );
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
    assert!(
        second.is_empty(),
        "the same minute must not fire a second time"
    );
    // The next minute (including the 8:00 daily job) naturally doesn't fire either.
    let later = due_now(&jobs, at(2026, 9, 21, 8, 1), &fired);
    assert!(later.is_empty());
}

/// A real firing: in-process Server, scripted model, one turn; assert the reply
/// lands in the outbox — the last mile (run_turn → file) is worth pinning.
#[tokio::test]
async fn firing_runs_a_turn_and_writes_the_reply_to_the_outbox() {
    use std::sync::Arc;

    use joyczl_provider::mock::Mock;
    use joyczl_provider::Resolved;

    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().to_path_buf();
    let settings = joyczl_config::Settings {
        home: home.clone(),
        api_key: Some("dummy".to_string()),
        ..Default::default()
    };
    let server = joyczl_app_server::open(&settings).await.expect("assemble");
    server.install_provider(Resolved {
        vision: false,
        provider_id: "mock".to_string(),
        client: Arc::new(Mock::new(vec![
            // The retrieval gate, then the loop's reply.
            Mock::text(r#"{"retrieve": false, "query": "", "reason": "routine"}"#),
            Mock::text("check-in done."),
        ])),
        model: "test".to_string(),
        small_model: "test-small".to_string(),
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
    });

    let job = job("tick", "* * * * *");
    super::fire(&server, &job, at(2026, 9, 21, 9, 0))
        .await
        .expect("the trigger succeeds");

    let path = home.join("outbox").join("schedule-tick-20260921-0900.md");
    let written = std::fs::read_to_string(&path).expect("the result file");
    assert!(written.contains("check-in done."), "{written}");
    assert!(
        written.contains("cron '* * * * *'"),
        "it has to say where the trigger came from: {written}"
    );

    // Scheduled sessions are separate from user sessions: this run's history lands in schedule:tick.
    let sessions = server.chat().sessions().await.expect("the session list");
    assert!(
        sessions.iter().any(|s| s.id == "schedule:tick"),
        "a scheduled job needs a session of its own: {sessions:?}"
    );
}

#[test]
fn jobs_load_from_both_skills_and_the_json_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path();
    // A skill with a schedule.
    let skill_dir = home.join("skills").join("weekly-review");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: weekly-review\ndescription: summarize the week\nschedule: 0 8 * * 1\n---\nsteps",
    )
    .unwrap();
    // A skill without a schedule: must not become a job.
    let plain = home.join("skills").join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(
        plain.join("SKILL.md"),
        "---\nname: plain\ndescription: nothing scheduled\n---\nsteps",
    )
    .unwrap();
    // Two entries in schedules.json (one broken).
    std::fs::write(
        home.join("schedules.json"),
        r#"{"jobs":[
            {"name":"standup","cron":"0 9 * * 1-5","prompt":"what is on today?"},
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
        "a skill job's request has to carry its own description: {}",
        weekly.prompt
    );
}

// ---- 7x24: state persistence and catch-up windows ------------------------------------------

/// The legacy state file reads back (one-time import source); a broken file counts
/// as absent — it only carries dedup info; losing it costs at most one duplicate.
#[test]
fn the_legacy_state_file_is_readable_but_only_once() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut fired = HashMap::new();
    fired.insert(
        "morning briefing".to_string(),
        "2026-10-27T08:00".to_string(),
    );
    let text = serde_json::to_string(&fired).expect("serialise");
    std::fs::write(dir.path().join("schedules-state.json"), text).expect("write the file");

    let loaded = load_legacy_state(dir.path());
    assert_eq!(
        loaded.get("morning briefing").map(String::as_str),
        Some("2026-10-27T08:00")
    );

    // A broken file counts as absent.
    std::fs::write(dir.path().join("schedules-state.json"), "{ half a thing")
        .expect("write the broken file");
    assert!(load_legacy_state(dir.path()).is_empty());
}

/// schedules.json's per-job policy fields: catch_up and delivery both parse; an
/// unknown delivery falls back to outbox (not silently — stderr says so).
#[test]
fn per_job_policy_fields_are_parsed() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("schedules.json"),
        r#"{"jobs": [
            {"name": "morning briefing", "cron": "0 8 * * *", "prompt": "morning", "catch_up": false, "delivery": "notify"},
            {"name": "watch CI", "cron": "*/15 * * * *", "prompt": "have a look", "delivery": "quiet"},
            {"name": "default", "cron": "0 12 * * *", "prompt": "noon"},
            {"name": "misspelt", "cron": "0 13 * * *", "prompt": "noon", "delivery": "a daily report"}
        ]}"#,
    )
    .expect("write the declaration");
    let jobs = load_jobs(dir.path());
    assert_eq!(jobs.len(), 4);
    let morning = jobs
        .iter()
        .find(|j| j.name == "morning briefing")
        .expect("the morning job");
    assert!(!morning.catch_up, "catch_up=false means at most once");
    assert_eq!(morning.delivery, Delivery::Notify);
    let ci = jobs
        .iter()
        .find(|j| j.name == "watch CI")
        .expect("the CI watcher");
    assert_eq!(ci.delivery, Delivery::Quiet);
    let plain = jobs
        .iter()
        .find(|j| j.name == "default")
        .expect("the default one");
    assert!(plain.catch_up, "by default, at least once");
    assert_eq!(plain.delivery, Delivery::Outbox);
    let wrong = jobs
        .iter()
        .find(|j| j.name == "misspelt")
        .expect("the misspelt one");
    assert_eq!(
        wrong.delivery,
        Delivery::Outbox,
        "an unknown value falls back"
    );
}

/// A miss in the downtime window → catch up the latest; no miss → nothing; three days back → latest only.
#[test]
fn a_missed_fire_is_caught_up_once() {
    let daily = job("morning briefing", "0 8 * * *");

    // Downtime window 7:00-9:00 covers 8:00 → catch up 8:00.
    let missed = last_missed(&daily, at(2026, 10, 27, 7, 0), at(2026, 10, 27, 9, 0));
    assert_eq!(missed, Some(at(2026, 10, 27, 8, 0)));

    // No trigger in the window → no catch-up.
    assert_eq!(
        last_missed(&daily, at(2026, 10, 27, 9, 0), at(2026, 10, 27, 9, 30)),
        None
    );

    // The start itself counts as missed (last fired 7:59; the 8:00 point is "missed", not "fired").
    let missed = last_missed(&daily, at(2026, 10, 26, 7, 59), at(2026, 10, 27, 8, 30));
    assert_eq!(
        missed,
        Some(at(2026, 10, 27, 8, 0)),
        "three days down, still only the newest is caught up"
    );
}

/// A per-minute job yields only the last missed minute even in a long downtime window.
#[test]
fn an_every_minute_job_yields_only_the_latest_missed_minute() {
    let frequent = job("heartbeat", "* * * * *");
    let missed = last_missed(&frequent, at(2026, 10, 20, 0, 0), at(2026, 10, 27, 0, 0));
    assert_eq!(missed, Some(at(2026, 10, 27, 0, 0)));
}

/// Ledger line shape: all seven fields present; status is one of ok/empty/error.
#[test]
fn the_run_log_line_has_all_the_fields() {
    let line = super::run_log_line("morning briefing", "2026-10-30T08:00", "ok", "", 42, true);
    let parsed: serde_json::Value = serde_json::from_str(&line).expect("legal JSON");
    for key in [
        "ts",
        "name",
        "minute",
        "status",
        "detail",
        "reply_chars",
        "delivered",
    ] {
        assert!(parsed.get(key).is_some(), "missing field {key}");
    }
    assert_eq!(parsed["name"], "morning briefing");
    assert_eq!(parsed["minute"], "2026-10-30T08:00");
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["reply_chars"], 42);
    assert_eq!(parsed["delivered"], true);

    let err = super::run_log_line(
        "watch CI",
        "2026-10-30T09:00",
        "error",
        "the turn failed: timed out",
        0,
        false,
    );
    let parsed: serde_json::Value = serde_json::from_str(&err).expect("legal JSON");
    assert_eq!(parsed["status"], "error");
    assert_eq!(parsed["detail"], "the turn failed: timed out");
    assert_eq!(parsed["delivered"], false);
}

/// Ledger append: new line after old ones; write failure doesn't blow up (a closed dir only notes stderr).
#[test]
fn the_run_log_appends_and_never_panics() {
    let dir = tempfile::tempdir().expect("temp dir");
    super::append_run_log(
        dir.path(),
        &super::run_log_line("a", "m1", "ok", "", 1, true),
    );
    super::append_run_log(
        dir.path(),
        &super::run_log_line("b", "m2", "empty", "", 0, false),
    );

    let text =
        std::fs::read_to_string(dir.path().join("schedule-runs.jsonl")).expect("the ledger exists");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "two appends make two lines");
    assert!(
        lines[0].contains("\"name\":\"a\""),
        "the older line is still there"
    );
    assert!(
        lines[1].contains("\"status\":\"empty\""),
        "the new line comes after it"
    );

    // Unopenable path: stderr only, no panic, no error — the ledger is a side path.
    super::append_run_log(dir.path().join("no such directory").as_path(), "x");
}

#[tokio::test]
async fn an_uncertain_trigger_is_not_replayed_and_requires_explicit_review() {
    if !joyczl_tools::exec::sandbox_available() {
        return;
    }
    use joyczl_provider::{mock::Mock, Resolved};
    use std::sync::Arc;
    let home = tempfile::tempdir().unwrap();
    let effects = tempfile::tempdir().unwrap();
    let marker = effects.path().join("committed.txt");
    let settings = joyczl_config::Settings {
        home: home.path().into(),
        api_key: Some("dummy".into()),
        exec_enabled: true,
        exec_allow: vec!["*".into()],
        exec_timeout_secs: 1,
        ..Default::default()
    };
    let server = joyczl_app_server::open(&settings).await.unwrap();
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve":false,"query":"","reason":"test"}"#),
        Mock::tool_use(
            "effect",
            "run_command",
            serde_json::json!({"command":format!("printf committed >> '{}'; sleep 3",marker.display()),"cwd":effects.path()}),
        ),
        Mock::text("Must not be reached"),
    ]));
    server.install_provider(Resolved {
        vision: false,
        provider_id: "mock".into(),
        client: mock.clone(),
        model: "test".into(),
        small_model: "test".into(),
        context_window: 100_000,
    });
    let job = job("uncertain", "* * * * *");
    let error = super::fire(&server, &job, at(2026, 11, 13, 8, 0))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("requires review"), "{error}");
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "committed");
    assert_eq!(mock.received.lock().unwrap().len(), 2);
    super::fire(&server, &job, at(2026, 11, 13, 8, 1))
        .await
        .unwrap();
    assert_eq!(mock.received.lock().unwrap().len(), 2);
    let schedules = joyczl_state::Schedules::new(server.state_pool());
    assert_eq!(
        schedules
            .pending_claim("uncertain")
            .await
            .unwrap()
            .as_deref(),
        Some("2026-11-13T08:00")
    );
    super::acknowledge(&server, "uncertain").await.unwrap();
    assert!(schedules
        .pending_claim("uncertain")
        .await
        .unwrap()
        .is_none());
    assert_eq!(mock.received.lock().unwrap().len(), 2);
}
