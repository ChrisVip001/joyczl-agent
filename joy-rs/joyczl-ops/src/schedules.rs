//! The capability center's third list: scheduled jobs, as declarations plus runtime state.
//!
//! Two sources declare a job (`joyczl-cli`'s `schedule.rs` reads both, and so does the model's
//! `schedule_job` tool through the same crate):
//!
//! * a skill whose SKILL.md frontmatter carries `schedule: 0 8 * * 1-5` — the job **is** the
//!   skill, so it is read-only here: deleting that row means deleting the skill or editing
//!   its frontmatter, and the row says so instead of offering a button that would lie;
//! * `<home>/schedules.json` — one-off jobs, and the only writable set.
//!
//! Runtime state (`last_fired`, `claimed`) lives in state.db and arrives from `schedule/list`;
//! this module never fabricates it. "Never ran" is `None`, not a zero timestamp.
//!
//! The file half lives in `joyczl-schedule` because three callers share it — this panel, the
//! scheduler, and the agent's tools. This module adds what only the panel knows: the merged
//! view, and the "next run" column.

use std::path::Path;

use chrono::NaiveDateTime;
use joyczl_protocol::ScheduleStateRow;
use joyczl_schedule::{declarations, next_run};

/// The save body is the crate's, not a second shape (the tool layer posts the same fields).
pub use joyczl_schedule::declarations::JobBody;

/// One scheduled job as the capability center shows it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRow {
    pub name: String,
    /// Five-field cron, exactly as declared.
    pub cron: String,
    pub prompt: String,
    /// outbox / notify / quiet — where the result goes.
    pub delivery: String,
    /// Re-run once after a miss (`true`) or give up (`false`).
    pub catch_up: bool,
    /// `file` (`<home>/schedules.json`, writable here) or `skill` (read-only here).
    pub source: String,
    /// The skill this job comes from, when `source == "skill"`.
    pub skill: Option<String>,
    pub last_fired_minute: Option<String>,
    /// Non-null = the last run was interrupted; the scheduler decides on restart.
    pub claimed_minute: Option<String>,
    /// The next fire, or `null` when the pattern is malformed / can never match.
    pub next_run: Option<String>,
    /// Why `next_run` is null, when it is `null` for an interesting reason.
    pub next_run_note: Option<String>,
}

/// `GET /api/schedules/jobs` — the list plus why `schedules.json` could not be read.
#[derive(Debug, serde::Serialize)]
pub struct SchedulesView {
    pub jobs: Vec<JobRow>,
    pub error: Option<String>,
}

/// Every job the machine declares, merged with the scheduler's state.
///
/// The declaration is what the panel lists, because that is what will actually fire; a state
/// row without a declaration is history, not a job.
pub fn rows(
    home: &Path,
    state: &[ScheduleStateRow],
    now: NaiveDateTime,
) -> (Vec<JobRow>, Option<String>) {
    let (mut jobs, error) = declarations_of(home);
    for job in &mut jobs {
        if let Some(row) = state.iter().find(|row| row.name == job.name) {
            job.last_fired_minute = row.last_fired_minute.clone();
            job.claimed_minute = row.claimed_minute.clone();
            // Declaration wins for policy: `schedule/list` echoes whatever was last
            // registered, and a file edited since then is the newer truth.
        }
        let (next, note) = next_run(&job.cron, now)
            .map_or_else(|why| (None, Some(why)), |when| (Some(when), None));
        job.next_run = next;
        job.next_run_note = note;
    }
    (jobs, error)
}

/// The two declaration sources, in the order the scheduler loads them.
fn declarations_of(home: &Path) -> (Vec<JobRow>, Option<String>) {
    let (skills, reserved) = skill_jobs(home);
    let mut jobs = skills;

    let (file_jobs, error) = declarations::read(home);
    for job in file_jobs {
        jobs.push(JobRow {
            name: job.name,
            cron: job.cron,
            prompt: job.prompt,
            delivery: job.delivery,
            catch_up: job.catch_up,
            source: "file".to_string(),
            skill: None,
            last_fired_minute: None,
            claimed_minute: None,
            next_run: None,
            next_run_note: None,
        });
    }
    let _ = reserved; // only the writers need it
    jobs.sort_by(|left, right| left.name.cmp(&right.name));
    (jobs, error)
}

/// Skill-declared jobs, plus the names a file job may not take.
fn skill_jobs(home: &Path) -> (Vec<JobRow>, Vec<String>) {
    let mut rows = Vec::new();
    let mut reserved = Vec::new();
    for skill in joyczl_memory::skills::loaded_skills(home) {
        let Some(cron) = skill.schedule.clone() else {
            continue;
        };
        reserved.push(skill.name.clone());
        rows.push(JobRow {
            name: skill.name.clone(),
            cron,
            prompt: format!(
                "按技能 '{}' 的做法执行一次：{}",
                skill.name, skill.description
            ),
            delivery: "outbox".to_string(),
            catch_up: true,
            source: "skill".to_string(),
            skill: Some(skill.name.clone()),
            last_fired_minute: None,
            claimed_minute: None,
            next_run: None,
            next_run_note: None,
        });
    }
    (rows, reserved)
}

/// Names that already mean a skill's job — a file row may not shadow one.
fn reserved(home: &Path) -> Vec<String> {
    skill_jobs(home).1
}

/// Add or replace one job in `<home>/schedules.json`.
pub fn save(home: &Path, body: &declarations::JobBody) -> Result<String, String> {
    declarations::save(home, body, &reserved(home))
}

/// Delete one file-declared job (skill jobs are refused with the way out).
pub fn remove(home: &Path, name: &str) -> Result<String, String> {
    declarations::remove(home, name, &reserved(home))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 11, 12)
            .expect("日期")
            .and_hms_opt(9, 0, 0)
            .expect("时间")
    }

    fn body(name: &str, cron: &str) -> declarations::JobBody {
        declarations::JobBody {
            name: name.to_string(),
            cron: cron.to_string(),
            prompt: "写一份简报".to_string(),
            delivery: "outbox".to_string(),
            catch_up: None,
        }
    }

    #[test]
    fn a_saved_job_lists_with_its_next_run_and_state() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().to_path_buf();
        save(&home, &body("digest", "0 8 * * *")).expect("保存");

        let state = vec![ScheduleStateRow {
            name: "digest".to_string(),
            last_fired_minute: Some("2026-11-12T08:00".to_string()),
            claimed_minute: None,
            catch_up: true,
            delivery: "outbox".to_string(),
        }];
        let (jobs, error) = rows(&home, &state, now());
        assert!(error.is_none(), "{error:?}");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].source, "file");
        assert_eq!(
            jobs[0].last_fired_minute.as_deref(),
            Some("2026-11-12T08:00")
        );
        assert_eq!(jobs[0].next_run.as_deref(), Some("2026-11-13 08:00"));

        remove(&home, "digest").expect("删除");
        assert!(rows(&home, &[], now()).0.is_empty());
    }

    #[test]
    fn an_impossible_pattern_shows_why_instead_of_a_blank() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().to_path_buf();
        save(&home, &body("never", "0 0 30 2 *")).expect("保存");
        let (jobs, _) = rows(&home, &[], now());
        assert_eq!(jobs[0].next_run, None);
        assert!(jobs[0]
            .next_run_note
            .as_deref()
            .expect("要有说明")
            .contains("一年内"));
    }
}
