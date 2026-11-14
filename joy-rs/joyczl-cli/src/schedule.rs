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
//! requires human reconciliation. `catch_up` applies only to missed triggers that were never claimed.
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
use chrono::{Local, NaiveDateTime};
use joyczl_app_server::Server;
#[path = "schedule_fire.rs"]
mod execution;
use execution::fire;

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
                eprintln!(
                    "(joy) unknown delivery '{other}' (outbox/notify/quiet); treating it as outbox"
                );
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
    /// Catch up missed, unclaimed triggers. Interrupted claims require human review.
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
                    "Run skill '{name}' once the way it says: {description}",
                    name = skill.name,
                    description = skill.description
                ),
                catch_up: true,
                delivery: Delivery::Outbox,
            });
        }
    }

    let (file_jobs, error) = joyczl_schedule::declarations::read(home);
    if let Some(error) = error {
        eprintln!("(joy) {error}");
    }
    jobs.extend(file_jobs.into_iter().map(|job| Job {
        name: job.name,
        cron: job.cron,
        prompt: job.prompt,
        catch_up: job.catch_up,
        delivery: Delivery::parse(&job.delivery),
    }));

    jobs
}

/// Five-field cron match — **implemented in `joyczl-app-server`** and re-exported here.
///
/// The capability center shows "next run" for the same patterns this process fires on, and
/// two matchers would drift into "the panel says 08:00, nothing ran at 08:00". One copy, two
/// callers; the tests below test the shared one.
pub use joyczl_schedule::matches as cron_matches;

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
                eprintln!(
                    "(joy) could not write the run ledger in {}: {e}",
                    path.display()
                );
            }
        }
        Err(e) => eprintln!(
            "(joy) could not open the run ledger {}: {e}",
            path.display()
        ),
    }
}

/// Whether a trigger was missed from `from` to `to` (inclusive, per-minute scan).
/// Returns the **last** missed point — down three days, catch up once, not a chain.
pub fn last_missed(job: &Job, from: NaiveDateTime, to: NaiveDateTime) -> Option<NaiveDateTime> {
    let mut latest = None;
    let mut t = from;
    let step = chrono::TimeDelta::try_minutes(1).expect("once a minute is always valid");
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
            "(joy) no scheduled jobs. Add a `schedule: 0 8 * * 1-5` line to a SKILL.md frontmatter, \
             or write jobs in {}.",
            home.join("schedules.json").display()
        );
    } else {
        for job in &initial {
            eprintln!("(joy) scheduled job {} — cron '{}'", job.name, job.cron);
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
                "(joy) could not record the state of job '{}' (continuing as if there were no history): {e}",
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
                        "(joy) imported the history of job '{}' from schedules-state.json into state.db",
                        job.name
                    );
                }
            }
        }

        // An interrupted claim may already have committed effects. Catch-up is for missed triggers only.
        match schedules.pending_claim(&job.name).await {
            Ok(Some(claimed)) => {
                eprintln!("(joy) job '{}' has an unsettled trigger at {claimed}; review external effects, then use joy schedule acknowledge <name>. It is not replayed.",job.name);
                fired.insert(job.name.clone(), now0.format("%Y-%m-%dT%H:%M").to_string());
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!(
                    "(joy) schedule claim could not be checked; skipping '{}': {error}",
                    job.name
                );
                continue;
            }
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
            let from = (last
                + chrono::TimeDelta::try_minutes(1).expect("once a minute is always valid"))
            .max(now0 - chrono::TimeDelta::try_days(7).expect("once a week is always valid"));
            if let Some(missed) = job
                .catch_up
                .then(|| last_missed(&job, from, now0))
                .flatten()
            {
                eprintln!(
                    "(joy) job '{}' missed its {} trigger; catching up now",
                    job.name,
                    missed.format("%Y-%m-%d %H:%M")
                );
                let catch_up = Job {
                    prompt: format!(
                        "{}\n\n(catch-up: the {} trigger was missed — the process was not running then.)",
                        job.prompt,
                        missed.format("%Y-%m-%d %H:%M")
                    ),
                    ..job.clone()
                };
                let minute = now0.format("%Y-%m-%dT%H:%M").to_string();
                if let Err(e) = fire(&server, &catch_up, now0).await {
                    eprintln!("(joy) the catch-up run of job '{}' failed: {e}", job.name);
                }
                fired.insert(job.name.clone(), minute);
            }
        }
    }

    loop {
        server.flush_completion_notices().await;
        let at = Local::now().naive_local();
        let jobs = load_jobs(&home);
        for job in due_now(&jobs, at, &fired) {
            let minute = at.format("%Y-%m-%dT%H:%M").to_string();
            // fire owns the durable claim; this cache prevents repeated attempts in one tick minute.
            fired.insert(job.name.clone(), minute.clone());
            // Serial execution: other jobs in the same minute wait for it to finish.
            // Hammering the model with scheduled jobs isn't "fast" — nobody can read it.
            if let Err(e) = fire(&server, job, at).await {
                eprintln!("(joy) job '{}' failed: {e}", job.name);
            }
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

/// Human review releases an interrupted slot without rerunning its work.
pub async fn acknowledge(server: &Server, name: &str) -> Result<()> {
    let rows = joyczl_state::schedule_completions::ScheduleCompletions::new(server.state_pool());
    match rows.acknowledge(name).await? {
        Some(minute) => {
            append_run_log(
                &server.settings().home,
                &run_log_line(
                    name,
                    &minute,
                    "reviewed",
                    "Human acknowledged the prior effects; no replay",
                    0,
                    false,
                ),
            );
            println!("Acknowledged '{name}' at {minute}; the old trigger will not be replayed.");
        }
        None => println!("No unsettled trigger for '{name}'."),
    }
    Ok(())
}

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod schedule_tests;
