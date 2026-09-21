//! `joy schedule` —— 声明式定时任务，常驻进程。
//!
//! 两条声明来源，同一套语义：
//!
//! * **技能**：SKILL.md 的 frontmatter 里加一行 `schedule: 0 8 * * 1-5`。
//!   技能本来就是「怎么做事」，加一行时间就成了「每周一早上八点这么做」——
//!   定时任务不需要另立一套格式。
//! * **`<home>/schedules.json`**：一次性列几条不适合写进技能的活。
//!
//! 触发时按**同一个会话跑一轮**（`schedule:<名字>`），所以每次执行都带着
//! 上一次的上下文与滚动摘要；结果写进 `<home>/outbox/`，不往聊天里塞 ——
//! 定时任务产出的是可以慢慢读的文件，不是打扰。
//!
//! 每分钟最多触发一次（`last_fired` 记到分钟），任务串行执行：同一分钟的
//! 多条任务依次跑，不会同时把模型打爆。不开这个进程也完全可用 ——
//! 用系统 cron 调 `joy gather` 是等价的轻量姿势。
//!
//! cron 是**手写的五字段匹配**（分 时 日 月 周），支持 `*`、`*/步长`、
//! `a-b` 区间、`a,b` 列表 —— 覆盖个人任务的全部需要，不为一个字段引依赖。

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use chrono::{Datelike, Local, NaiveDateTime, Timelike};
use joyczl_app_server::{run_turn, EventSink, Frame, Server};
use joyczl_protocol::{RequestId, ServerNotification, TurnStartParams};

/// 一条定时任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub name: String,
    /// 五字段 cron：`分 时 日 月 周`。
    pub cron: String,
    /// 触发时发给 Joy 的那句话。
    pub prompt: String,
}

/// 从技能与 `schedules.json` 收集任务。读不出来的条目跳过并警告 ——
/// 一条写坏的声明不该让整个调度器起不来。
pub fn load_jobs(home: &Path) -> Vec<Job> {
    let mut jobs = Vec::new();

    // 技能里的 schedule 行。
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

/// 五字段 cron 匹配（分 时 日 月 周；周 = 0..=6，周日是 0，也认 7）。
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
    // 列表：任何一段命中就算。
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
        // 单个数字。周日的 7 归一成 0。
        match part.parse::<u32>() {
            Ok(n) => {
                let n = if min == 0 && max == 6 && n == 7 { 0 } else { n };
                n == value
            }
            Err(_) => false,
        }
    })
}

/// 这一分钟该触发哪些任务。`fired` 记「任务 → 上次触发的分钟」，
/// 同一个任务同一分钟只会被交出来一次。
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

/// 常驻循环：每 20 秒看一眼现在该不该触发。技能改动每次 tick 重新扫，
/// 等于热加载。
pub async fn run(server: Server) -> Result<()> {
    let home = server.settings().home.clone();
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

    loop {
        let at = Local::now().naive_local();
        let jobs = load_jobs(&home);
        for job in due_now(&jobs, at, &fired) {
            fired.insert(job.name.clone(), at.format("%Y-%m-%dT%H:%M").to_string());
            // 串行执行：同一分钟的其它任务等它跑完。定时任务打爆模型
            // 不是「快」，是没人读得过来。
            if let Err(e) = fire(&server, job, at).await {
                eprintln!("(joy) 任务 '{}' 执行失败：{e}", job.name);
            }
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

/// 跑一轮并把结果写进 outbox。会话按任务名分开 —— 每次执行带着同一个
/// 任务的历史与滚动摘要，任务之间互不串味。
async fn fire(server: &Server, job: &Job, at: NaiveDateTime) -> Result<()> {
    eprintln!("(joy) 触发任务 '{}'（{}）", job.name, job.cron);
    let (sink, mut rx) = EventSink::channel();
    let params = TurnStartParams {
        session_id: Some(format!("schedule:{}", job.name)),
        message: job.prompt.clone(),
        stream: Some(true),
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
    task.await
        .map_err(|e| anyhow::anyhow!("任务 '{}' 的 turn 崩了：{e}", job.name))?
        .map_err(|e| anyhow::anyhow!("任务 '{}' 没跑通：{}", job.name, e.message))?;

    let home = server.settings().home;
    let stamp = at.format("%Y%m%d-%H%M");
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
    Ok(())
}

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod schedule_tests;
