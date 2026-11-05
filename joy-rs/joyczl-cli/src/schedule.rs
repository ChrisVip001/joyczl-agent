//! `joy schedule` — declarative scheduled jobs, a resident process.
//!
//! Two declaration sources, one shared semantics:
//!
//! * **Skills**: add a line `schedule: 0 8 * * 1-5` to the SKILL.md frontmatter.
//!   A skill already says "how to do things"; adding a time makes it "do this
//!   every weekday at 8am" — scheduled jobs need no separate format.
//! * **`<home>/schedules.json`**: list a few one-off jobs that don't fit a skill.
//!
//! On trigger it runs **one turn in the same session** (`schedule:<name>`), so each
//! execution carries the previous context and rolling summary; results go into
//! `<home>/outbox/`, not into chat — scheduled output is a file to read at leisure.
//!
//! At most one trigger per minute (`last_fired` records to the minute); jobs run
//! serially: same-minute tasks run one by one, never hammering the model at once.
//! The process is optional — driving `joy gather` via system cron is equivalent.
//!
//! cron is a **hand-written five-field matcher** (min hour day month weekday),
//! supporting `*`, `*/step`, `a-b` ranges, `a,b` lists — no dependency for one field.
//!
//! State and crash recovery (M4b: dispatch advances next_run first): schedule
//! state lives in state.db's `schedule_state` table (old schedules-state.json is
//! only a one-time import source now, no longer authoritative). Before firing,
//! `claim` the slot, then execute; after settling, `confirm` — a crash in between
//! is decided on restart by each job's `catch_up`: `catch_up=1` (report class)
//! re-runs once — at-least-once; `catch_up=0` (notify class) drops — at-most-once.
//!
//! Each job also has `delivery`: `outbox` (default, writes a file) | `notify` (file
//! plus notifier, for people away from the screen) | `quiet` (log only, so high-
//! frequency jobs don't spam the disk); empty replies (or `NO_REPLY`) skip delivery.
//!
//! Missed while down: at startup, look back over the minutes since the last fire
//! and **re-run only the latest one** (three days down still catches up once, not
//! a chain; lookback capped at seven days). Missed fires are not silently swallowed.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use chrono::{Datelike, Local, NaiveDateTime, Timelike};
use joyczl_app_server::{run_turn, EventSink, Frame, Server};
use joyczl_protocol::{RequestId, ServerNotification, TurnStartParams};

/// Where output is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Writes `<home>/outbox/` (default — scheduled output is a file read at leisure).
    Outbox,
    /// Writes the file **and** pushes a notifier (Slack/ntfy) — off-screen too.
    Notify,
    /// Logs only: high-frequency jobs (watching CI every 15 min) shouldn't spray files.
    Quiet,
}

impl Delivery {
    fn parse(text: &str) -> Delivery {
        match text.trim().to_ascii_lowercase().as_str() {
            "notify" => Delivery::Notify,
            "quiet" => Delivery::Quiet,
            "outbox" => Delivery::Outbox,
            other => {
                eprintln!("(joy) delivery '{other}' 不认识（outbox/notify/quiet），按 outbox 处理");
                Delivery::Outbox
            }
        }
    }
}

/// One scheduled job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub name: String,
    /// Five-field cron: `min hour day month weekday`.
    pub cron: String,
    /// The prompt sent to Joy when it fires.
    pub prompt: String,
    /// Catch up after a miss/interruption? Default true (at-least-once — reports prefer no loss).
    pub catch_up: bool,
    /// How output is delivered. Defaults to Outbox.
    pub delivery: Delivery,
}

/// Collects jobs from skills and `schedules.json`. Unreadable entries are skipped
/// with a warning — one broken declaration must not keep the scheduler from starting.
pub fn load_jobs(home: &Path) -> Vec<Job> {
    let mut jobs = Vec::new();

    // The schedule line inside skills.
    for skill in joyczl_memory::skills::loaded_skills(home) {
        if let Some(cron) = skill.schedule {
            jobs.push(Job {
                name: skill.name.clone(),
                cron,
                prompt: format!(
                    "按技能 '{name}' 的做法执行一次：{description}",
                    name = skill.name,
                    description = skill.description
                ),
                catch_up: true,
                delivery: Delivery::Outbox,
            });
        }
    }

    // schedules.json。
    let path = home.join("schedules.json");
    if let Ok(text) = std::fs::read_to_string(&path) {
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => {
                for entry in value
                    .get("jobs")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                {
                    let name = entry.get("name").and_then(|v| v.as_str());
                    let cron = entry.get("cron").and_then(|v| v.as_str());
                    let prompt = entry.get("prompt").and_then(|v| v.as_str());
                    match (name, cron, prompt) {
                        (Some(name), Some(cron), Some(prompt)) => jobs.push(Job {
                            name: name.to_string(),
                            cron: cron.to_string(),
                            prompt: prompt.to_string(),
                            catch_up: entry
                                .get("catch_up")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(true),
                            delivery: Delivery::parse(
                                entry
                                    .get("delivery")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("outbox"),
                            ),
                        }),
                        _ => eprintln!(
                            "(joy) {} 里有一条缺 name/cron/prompt 的任务，跳过",
                            path.display()
                        ),
                    }
                }
            }
            Err(e) => eprintln!("(joy) {} 不是合法 JSON，跳过：{e}", path.display()),
        }
    }

    jobs
}

/// Five-field cron match (min hour day month weekday; Sunday is 0, also accepts 7).
pub fn cron_matches(cron: &str, at: NaiveDateTime) -> bool {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    let weekday = at.weekday().num_days_from_sunday();
    field_matches(fields[0], at.minute(), 0, 59)
        && field_matches(fields[1], at.hour(), 0, 23)
        && field_matches(fields[2], at.day(), 1, 31)
        && field_matches(fields[3], at.month(), 1, 12)
        && field_matches(fields[4], weekday, 0, 6)
}

fn field_matches(field: &str, value: u32, min: u32, max: u32) -> bool {
    if field == "*" {
        return true;
    }
    // List: any segment hit counts.
    field.split(',').any(|part| {
        let part = part.trim();
        if let Some(step) = part.strip_prefix("*/") {
            return match step.parse::<u32>() {
                Ok(step) if step > 0 => value.is_multiple_of(step),
                _ => false,
            };
        }
        if let Some((from, to)) = part.split_once('-') {
            return match (from.trim().parse::<u32>(), to.trim().parse::<u32>()) {
                (Ok(from), Ok(to)) => value >= from && value <= to,
                _ => false,
            };
        }
        // Single number. Sunday's 7 normalizes to 0.
        match part.parse::<u32>() {
            Ok(n) => {
                let n = if min == 0 && max == 6 && n == 7 { 0 } else { n };
                n == value
            }
            Err(_) => false,
        }
    })
}

/// Which jobs should fire this minute. `fired` maps "job → minute last fired";
/// the same job is handed out only once per minute.
pub fn due_now<'a>(
    jobs: &'a [Job],
    at: NaiveDateTime,
    fired: &HashMap<String, String>,
) -> Vec<&'a Job> {
    let minute_key = at.format("%Y-%m-%dT%H:%M").to_string();
    jobs.iter()
        .filter(|job| cron_matches(&job.cron, at))
        .filter(|job| fired.get(&job.name) != Some(&minute_key))
        .collect()
}

/// The legacy state file (authoritative before M4b, now a **one-time import
/// source**). If it fails to parse, treat it as absent — it only ever held dedup info.
fn load_legacy_state(home: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(home.join("schedules-state.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// One ledger line per run (JSONL). The outbox is output (quiet has no output
/// at all); this ledger answers
/// "what ran and how each ended" — inspection, debugging, and frequency math.
fn run_log_line(
    name: &str,
    minute: &str,
    status: &str,
    detail: &str,
    reply_chars: usize,
    delivered: bool,
) -> String {
    let record = serde_json::json!({
        "ts": Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        "name": name,
        "minute": minute,
        "status": status,
        "detail": detail,
        "reply_chars": reply_chars,
        "delivered": delivered,
    });
    record.to_string()
}

/// Appends one line to `<home>/schedule-runs.jsonl`. Side path: a write failure
/// only reports to stderr — a broken ledger must not turn a success into an error.
fn append_run_log(home: &Path, line: &str) {
    use std::io::Write as _;
    let path = home.join("schedule-runs.jsonl");
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut file) => {
            if let Err(e) = writeln!(file, "{line}") {
                eprintln!("(joy) run 账本写不进 {}：{e}", path.display());
            }
        }
        Err(e) => eprintln!("(joy) run 账本打不开 {}：{e}", path.display()),
    }
}

/// Whether a trigger was missed from `from` to `to` (inclusive, per-minute scan).
/// Returns the **last** missed point — down three days, catch up once, not a chain.
pub fn last_missed(job: &Job, from: NaiveDateTime, to: NaiveDateTime) -> Option<NaiveDateTime> {
    let mut latest = None;
    let mut t = from;
    let step = chrono::TimeDelta::try_minutes(1).expect("一分钟总是合法");
    while t <= to {
        if cron_matches(&job.cron, t) {
            latest = Some(t);
        }
        t += step;
    }
    latest
}

/// Resident loop: every 20 seconds, check whether anything should fire. Skill
/// changes are rescanned each tick — effectively hot reload.
pub async fn run(server: Server) -> Result<()> {
    let home = server.settings().home.clone();
    let schedules = joyczl_state::Schedules::new(server.state_pool());
    let legacy = load_legacy_state(&home);
    let mut fired: HashMap<String, String> = HashMap::new();

    let initial = load_jobs(&home);
    if initial.is_empty() {
        eprintln!(
            "(joy) 没有定时任务。给 SKILL.md 的 frontmatter 加一行 schedule: 0 8 * * 1-5，\
             或在 {} 里写 jobs。",
            home.join("schedules.json").display()
        );
    } else {
        for job in &initial {
            eprintln!("(joy) 定时任务 {} —— cron '{}'", job.name, job.cron);
        }
    }

    // Registration + one-time import + crash recovery + catch-up, all before the start.
    let now0 = Local::now().naive_local();
    for job in load_jobs(&home) {
        let delivery = match job.delivery {
            Delivery::Outbox => "outbox",
            Delivery::Notify => "notify",
            Delivery::Quiet => "quiet",
        };
        if let Err(e) = schedules.ensure(&job.name, job.catch_up, delivery).await {
            eprintln!(
                "(joy) 任务 '{}' 的状态登记失败（按无历史继续）：{e}",
                job.name
            );
            continue;
        }

        // One-time import: DB has no history but the old JSON does — move the row in; never re-read.
        if schedules
            .last_fired(&job.name)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            if let Some(minute) = legacy.get(&job.name) {
                if schedules.import_legacy(&job.name, minute).await.is_ok() {
                    eprintln!(
                        "(joy) 任务 '{}' 的历史从 schedules-state.json 导入 state.db",
                        job.name
                    );
                }
            }
        }

        // Crash recovery: claimed but unconfirmed = the process died mid-run last time.
        if let Ok(Some(claimed)) = schedules.pending_claim(&job.name).await {
            if job.catch_up {
                eprintln!(
                    "(joy) 任务 '{}' 上次执行中断在 {claimed}，按 catch_up=1 补跑一次",
                    job.name
                );
                let resumed = Job {
                    prompt: format!(
                        "{}\n\n（补跑：{claimed} 那次触发后进程中断了 —— 这次是重新执行。）",
                        job.prompt
                    ),
                    ..job.clone()
                };
                if let Err(e) = fire(&server, &resumed, now0).await {
                    eprintln!("(joy) 任务 '{}' 补跑失败：{e}", job.name);
                }
                let minute = now0.format("%Y-%m-%dT%H:%M").to_string();
                let _ = schedules.confirm(&job.name, &minute).await;
                fired.insert(job.name.clone(), minute);
            } else {
                eprintln!(
                    "(joy) 任务 '{}' 上次执行中断在 {claimed}，按 catch_up=0 放弃（至多一次）",
                    job.name
                );
                let _ = schedules.confirm(&job.name, &claimed).await;
                fired.insert(job.name.clone(), claimed);
            }
            continue;
        }

        // Regular catch-up: the latest trigger missed since the last confirm.
        if let Some(last) = schedules
            .last_fired(&job.name)
            .await
            .ok()
            .flatten()
            .and_then(|text| NaiveDateTime::parse_from_str(&text, "%Y-%m-%dT%H:%M").ok())
        {
            fired.insert(job.name.clone(), last.format("%Y-%m-%dT%H:%M").to_string());
            // Lookback capped at seven days: older misses aren't worth a million-minute scan.
            let from = (last + chrono::TimeDelta::try_minutes(1).expect("一分钟总是合法"))
                .max(now0 - chrono::TimeDelta::try_days(7).expect("七天总是合法"));
            if let Some(missed) = last_missed(&job, from, now0) {
                eprintln!(
                    "(joy) 任务 '{}' 在 {} 错过了触发，现在补跑",
                    job.name,
                    missed.format("%Y-%m-%d %H:%M")
                );
                let catch_up = Job {
                    prompt: format!(
                        "{}\n\n（补跑：{} 那次的定时触发没赶上 —— 进程当时没在跑。）",
                        job.prompt,
                        missed.format("%Y-%m-%d %H:%M")
                    ),
                    ..job.clone()
                };
                let minute = now0.format("%Y-%m-%dT%H:%M").to_string();
                let _ = schedules.claim(&job.name, &minute).await;
                if let Err(e) = fire(&server, &catch_up, now0).await {
                    eprintln!("(joy) 任务 '{}' 补跑失败：{e}", job.name);
                }
                let _ = schedules.confirm(&job.name, &minute).await;
                fired.insert(job.name.clone(), minute);
            }
        }
    }

    loop {
        let at = Local::now().naive_local();
        let jobs = load_jobs(&home);
        for job in due_now(&jobs, at, &fired) {
            let minute = at.format("%Y-%m-%dT%H:%M").to_string();
            // dispatch claims the slot before executing: a crash in between is handled by
            // the restart recovery logic explicitly (catch up or give up), not by luck.
            let _ = schedules.claim(&job.name, &minute).await;
            fired.insert(job.name.clone(), minute.clone());
            // Serial execution: other jobs in the same minute wait for it to finish.
            // Hammering the model with scheduled jobs isn't "fast" — nobody can read it.
            if let Err(e) = fire(&server, job, at).await {
                eprintln!("(joy) 任务 '{}' 执行失败：{e}", job.name);
            }
            // Confirm on both success and failure: no backoff exists, so failures mustn't retry every 20s.
            let _ = schedules.confirm(&job.name, &minute).await;
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

/// Runs one turn and writes the result to the outbox. Sessions are split by job
/// name — each execution carries that job's history and rolling summary.
async fn fire(server: &Server, job: &Job, at: NaiveDateTime) -> Result<()> {
    eprintln!("(joy) 触发任务 '{}'（{}）", job.name, job.cron);
    let (sink, mut rx) = EventSink::channel();
    let params = TurnStartParams {
        session_id: Some(format!("schedule:{}", job.name)),
        message: job.prompt.clone(),
        stream: Some(true),
        // No working directory for scheduled jobs: home semantics is safest (scripts use absolute paths).
        cwd: None,
        attachments: None,
        tier: None,
    };
    let server_for_turn = server.clone();
    let task = tokio::spawn(async move {
        run_turn(&server_for_turn, params, RequestId::Number(0), &sink).await
    });

    let mut reply = String::new();
    while let Some(frame) = rx.recv().await {
        if let Frame::Notification(ServerNotification::TurnCompleted(done)) = frame {
            reply = done.reply;
            break;
        }
    }
    // All three turn outcomes (completed / turn error / job crash) go through this
    // ledger — error is a first-class citizen; it's exactly the line inspection seeks.
    let home = server.settings().home;
    let minute_text = at.format("%Y-%m-%dT%H:%M").to_string();
    let turn_result = match task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("turn 报错：{}", e.message)),
        Err(e) => Err(format!("turn 任务崩了：{e}")),
    };
    if let Err(why) = turn_result {
        append_run_log(
            &home,
            &run_log_line(&job.name, &minute_text, "error", &why, 0, false),
        );
        anyhow::bail!("任务 '{}' 没跑通：{why}", job.name);
    }

    // An empty reply (or the model answering NO_REPLY by convention) isn't worth
    // delivering — an empty file or empty push is an interruption. One ledger line is enough.
    let trimmed = reply.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("no_reply") {
        eprintln!("(joy) 任务 '{}' 完成，但没有产出内容，跳过投递", job.name);
        append_run_log(
            &home,
            &run_log_line(&job.name, &minute_text, "empty", "", 0, false),
        );
        return Ok(());
    }

    let stamp = at.format("%Y%m%d-%H%M");
    if job.delivery != Delivery::Quiet {
        let path = home
            .join("outbox")
            .join(format!("schedule-{}-{stamp}.md", job.name));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &path,
            format!(
                "# {name}\n\n_由 joy schedule 在 {at} 按 cron '{cron}' 触发_\n\n## 请求\n\n{prompt}\n\n## 回答\n\n{reply}\n",
                name = job.name,
                at = at.format("%Y-%m-%d %H:%M"),
                cron = job.cron,
                prompt = job.prompt,
            ),
        )?;
        eprintln!(
            "(joy) 任务 '{}' 完成，结果写进 {}",
            job.name,
            path.display()
        );
    } else {
        eprintln!(
            "(joy) 任务 '{}' 完成（quiet：不写文件），回答 {} 字",
            job.name,
            trimmed.chars().count()
        );
    }

    // Notify delivery: off-screen people should know — side path; missing notifier or send failure never blocks.
    if job.delivery == Delivery::Notify {
        if let Some(notifier) = server.notifier() {
            notifier.notify(joyczl_tools::notify::NotifyEvent::ScheduleRan {
                name: job.name.clone(),
                status: "完成".to_string(),
                preview: trimmed.chars().take(120).collect(),
            });
        }
    }

    // Output accounting: delivered only means "wrote the outbox file" (quiet has
    // output but doesn't write it — different from empty, which has no output).
    append_run_log(
        &home,
        &run_log_line(
            &job.name,
            &minute_text,
            "ok",
            "",
            trimmed.chars().count(),
            job.delivery != Delivery::Quiet,
        ),
    );
    Ok(())
}

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod schedule_tests;
