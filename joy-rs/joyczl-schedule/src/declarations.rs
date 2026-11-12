//! `<home>/schedules.json`: the writable half of a scheduled job.
//!
//! The scheduler reads this file at boot (and on every tick for skill edits), the dashboard
//! edits it, and the model can create a job from a sentence — three writers would be three
//! chances to disagree, so all three go through [`save`] / [`remove`] here. Each refusal is a
//! sentence that names the thing to fix; that sentence goes straight to the panel or back to
//! the model, so it has to be actionable rather than a status code.
//!
//! Two invariants worth stating:
//!
//! * **A file that does not parse is never overwritten.** Silently rewriting it would delete
//!   every job the human had in it; the honest answer is "fix it first".
//! * **Unknown keys survive a save.** This is an editor for one row, not a rewrite of the
//!   user's file: other jobs, other top-level keys and fields a future build added all stay.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::cron;

/// The delivery modes the scheduler understands. Anything else is refused rather than
/// silently treated as `outbox`: a job whose result goes somewhere other than the human
/// expects is worse than a job that refuses to be saved.
pub const DELIVERIES: [&str; 3] = ["outbox", "notify", "quiet"];

/// One job exactly as the file declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileJob {
    pub name: String,
    pub cron: String,
    pub prompt: String,
    pub delivery: String,
    pub catch_up: bool,
}

/// The fields a caller may set.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct JobBody {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cron: String,
    #[serde(default)]
    pub prompt: String,
    /// Empty = `outbox` (the default the scheduler assumes).
    #[serde(default)]
    pub delivery: String,
    /// `None` = true: a missed report is worth re-running once.
    #[serde(default)]
    pub catch_up: Option<bool>,
}

pub fn path_of(home: &Path) -> PathBuf {
    home.join("schedules.json")
}

/// The file's jobs, plus why it could not be read.
///
/// A missing file is **not** an error — most installs have none. A broken one is, and it must
/// not look like "no jobs": the panel shows the reason, the scheduler prints it, and neither
/// pretends the shelf is empty.
pub fn read(home: &Path) -> (Vec<FileJob>, Option<String>) {
    let path = path_of(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), None),
        Err(error) => {
            return (
                Vec::new(),
                Some(format!("读不了 {}：{error}", path.display())),
            )
        }
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            return (
                Vec::new(),
                Some(format!(
                    "{} 不是合法 JSON（{error}）—— 不会覆盖它，先手工修好",
                    path.display()
                )),
            )
        }
    };
    let mut jobs = Vec::new();
    for entry in value
        .get("jobs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let name = entry.get("name").and_then(Value::as_str);
        let cron = entry.get("cron").and_then(Value::as_str);
        let prompt = entry.get("prompt").and_then(Value::as_str);
        let (Some(name), Some(cron), Some(prompt)) = (name, cron, prompt) else {
            // A half-written entry is skipped by the scheduler too; listing it here would
            // promise a fire that never happens.
            continue;
        };
        jobs.push(FileJob {
            name: name.to_string(),
            cron: cron.to_string(),
            prompt: prompt.to_string(),
            delivery: entry
                .get("delivery")
                .and_then(Value::as_str)
                .unwrap_or("outbox")
                .to_string(),
            catch_up: entry
                .get("catch_up")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        });
    }
    (jobs, None)
}

/// Add or replace one job.
///
/// `reserved` lists names that already mean something else — today, skills that declare their
/// own `schedule:` line. The scheduler keys jobs by name, so two declarations sharing one
/// would fight; refusing here says which one wins instead of letting the file silently lie.
pub fn save(home: &Path, body: &JobBody, reserved: &[String]) -> Result<String, String> {
    let name = body.name.trim();
    validate_name(name)?;
    if reserved.iter().any(|taken| taken == name) {
        return Err(format!(
            "「{name}」已经被一个带 schedule 的技能占用 —— 换个名字，或去改那个技能的 SKILL.md"
        ));
    }
    let cron = body.cron.split_whitespace().collect::<Vec<_>>().join(" ");
    if !crate::cron::valid(&cron) {
        return Err(format!(
            "cron「{}」不合法：五段，如 `0 8 * * 1-5`（工作日八点）",
            body.cron.trim()
        ));
    }
    let prompt = body.prompt.trim();
    if prompt.is_empty() {
        return Err("要写清楚触发时做什么 —— 这一句会作为一次对话发给 Joy".to_string());
    }
    let delivery = if body.delivery.trim().is_empty() {
        "outbox".to_string()
    } else {
        body.delivery.trim().to_ascii_lowercase()
    };
    if !DELIVERIES.contains(&delivery.as_str()) {
        return Err(format!(
            "投递方式「{delivery}」不认识：outbox（写文件）/ notify（文件 + 通知）/ quiet（只记日志）"
        ));
    }

    let path = path_of(home);
    let mut root = match read_root(&path)? {
        Some(value) => value,
        None => Value::Object(Map::new()),
    };
    let object = match root.as_object_mut() {
        Some(object) => object,
        None => return Err(format!("{} 的顶层不是对象，先手工修好", path.display())),
    };
    let jobs = object
        .entry("jobs")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(array) = jobs.as_array_mut() else {
        return Err(format!("{} 里的 jobs 不是数组，先手工修好", path.display()));
    };

    let catch_up = body.catch_up.unwrap_or(true);
    let build = |existing: Option<&Value>| -> Value {
        // Keep keys this build does not know: a future field, or a note the human added.
        let mut entry = existing
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        entry.insert("name".to_string(), Value::String(name.to_string()));
        entry.insert("cron".to_string(), Value::String(cron.clone()));
        entry.insert("prompt".to_string(), Value::String(prompt.to_string()));
        entry.insert("delivery".to_string(), Value::String(delivery.clone()));
        entry.insert("catch_up".to_string(), Value::Bool(catch_up));
        Value::Object(entry)
    };

    match array
        .iter_mut()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
    {
        Some(slot) => *slot = build(Some(slot)),
        None => array.push(build(None)),
    }

    write_root(&path, &root)?;
    Ok(format!("已保存「{name}」：{cron}，投递 {delivery}"))
}

/// Delete one file-declared job. A name a skill owns is refused with the way out.
pub fn remove(home: &Path, name: &str, reserved: &[String]) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("要删哪一个？名字是空的".to_string());
    }
    if reserved.iter().any(|taken| taken == name) {
        return Err(format!(
            "「{name}」随技能走 —— 删掉那个技能，或把 SKILL.md 里的 schedule 行去掉"
        ));
    }

    let path = path_of(home);
    let mut root =
        read_root(&path)?.ok_or_else(|| format!("{} 还不存在，没有可删的任务", path.display()))?;
    let Some(array) = root.get_mut("jobs").and_then(Value::as_array_mut) else {
        return Err(format!("{} 里没有 jobs 数组", path.display()));
    };
    let before = array.len();
    array.retain(|entry| entry.get("name").and_then(Value::as_str) != Some(name));
    if array.len() == before {
        return Err(format!("schedules.json 里没有「{name}」"));
    }
    write_root(&path, &root)?;
    Ok(format!("已删除「{name}」"))
}

/// Lowercase letters, digits and hyphens — the name is the scheduler's key, and it travels
/// into `schedule:<name>` session ids.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("任务要有名字：小写字母、数字、连字符，如 morning-news".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "名字「{name}」不合法：只能用小写字母、数字和连字符（它也是日程的键）"
        ));
    }
    if name.len() > 64 {
        return Err("名字太长了（最多 64 个字符）".to_string());
    }
    Ok(())
}

/// The file's JSON, or `None` when it does not exist yet.
fn read_root(path: &Path) -> Result<Option<Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|error| {
            format!(
                "{} 不是合法 JSON（{error}）—— 为了不覆盖它，这次不写",
                path.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读不了 {}：{error}", path.display())),
    }
}

/// Write through a temp file in the same directory, then rename: a crash mid-write leaves
/// the old file intact instead of a truncated one the scheduler would refuse to parse.
fn write_root(path: &Path, root: &Value) -> Result<(), String> {
    let text =
        serde_json::to_string_pretty(root).map_err(|error| format!("序列化失败：{error}"))?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, format!("{text}\n"))
        .map_err(|error| format!("写不了 {}：{error}", temp.display()))?;
    std::fs::rename(&temp, path).map_err(|error| format!("改名失败：{error}"))
}

/// The next fire as `YYYY-MM-DD HH:MM`, or the reason there is none.
///
/// One formatter for the panel and the model's answer: two would eventually disagree about
/// what "next" means the second the clock passes a minute.
pub fn next_run_at(cron: &str, from: chrono::NaiveDateTime) -> Result<String, String> {
    cron::next_run(cron, from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().to_path_buf();
        (dir, home)
    }

    fn body(name: &str, cron: &str) -> JobBody {
        JobBody {
            name: name.to_string(),
            cron: cron.to_string(),
            prompt: "写一份简报".to_string(),
            delivery: "outbox".to_string(),
            catch_up: None,
        }
    }

    #[test]
    fn save_then_read_then_remove_roundtrips() {
        let (_dir, home) = home();
        save(&home, &body("morning-news", "0 8 * * *"), &[]).expect("保存");
        let (jobs, error) = read(&home);
        assert!(error.is_none(), "{error:?}");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "morning-news");
        assert!(jobs[0].catch_up, "默认补跑");

        remove(&home, "morning-news", &[]).expect("删除");
        assert!(read(&home).0.is_empty());
    }

    #[test]
    fn a_second_save_replaces_instead_of_appending() {
        let (_dir, home) = home();
        save(&home, &body("digest", "0 8 * * *"), &[]).expect("保存");
        let mut changed = body("digest", "30 19 * * *");
        changed.prompt = "晚上的那一份".to_string();
        save(&home, &changed, &[]).expect("覆盖");
        let jobs = read(&home).0;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].cron, "30 19 * * *");
        assert_eq!(jobs[0].prompt, "晚上的那一份");
    }

    #[test]
    fn unknown_keys_and_other_jobs_survive() {
        let (_dir, home) = home();
        std::fs::write(
            home.join("schedules.json"),
            r#"{"note":"我自己的备注","jobs":[
                {"name":"a","cron":"0 8 * * *","prompt":"x","tag":"手写"},
                {"name":"b","cron":"0 9 * * *","prompt":"y"}]}"#,
        )
        .expect("写");
        save(&home, &body("a", "0 10 * * *"), &[]).expect("覆盖");

        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(home.join("schedules.json")).expect("读"),
        )
        .expect("解析");
        assert_eq!(value["note"], "我自己的备注");
        assert_eq!(value["jobs"][0]["tag"], "手写");
        assert_eq!(value["jobs"][0]["cron"], "0 10 * * *");
        assert_eq!(value["jobs"][1]["name"], "b", "别的任务不能被这次保存弄丢");
    }

    #[test]
    fn bad_input_is_refused_with_a_sentence() {
        let (_dir, home) = home();
        assert!(save(&home, &body("Bad Name", "0 8 * * *"), &[]).is_err());
        assert!(save(&home, &body("ok", "0 8 * *"), &[])
            .unwrap_err()
            .contains("五段"));
        let mut empty = body("ok", "0 8 * * *");
        empty.prompt = "  ".to_string();
        assert!(save(&home, &empty, &[]).unwrap_err().contains("做什么"));
        let mut weird = body("ok", "0 8 * * *");
        weird.delivery = "telepathy".to_string();
        assert!(save(&home, &weird, &[]).unwrap_err().contains("投递方式"));
        assert!(remove(&home, "nothing", &[]).is_err());
    }

    #[test]
    fn a_name_a_skill_owns_is_reserved() {
        let (_dir, home) = home();
        let reserved = vec!["weekly-review".to_string()];
        assert!(save(&home, &body("weekly-review", "0 8 * * 1"), &reserved).is_err());
        // 同名文件任务已经存在时也不能删 —— 它属于技能。
        assert!(remove(&home, "weekly-review", &reserved).is_err());
        // 别的名字照常。
        assert!(save(&home, &body("other", "0 8 * * 1"), &reserved).is_ok());
    }

    #[test]
    fn a_broken_file_is_never_overwritten() {
        let (_dir, home) = home();
        std::fs::write(home.join("schedules.json"), "{ not json").expect("写");
        assert!(save(&home, &body("a", "0 8 * * *"), &[]).is_err());
        assert_eq!(
            std::fs::read_to_string(home.join("schedules.json")).expect("读"),
            "{ not json"
        );
    }
}
