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
//!
//! 状态与崩溃恢复（M4b，抄 openhuman 的「dispatch 先推进 next_run」）：
//! 调度状态在 state.db 的 `schedule_state` 表（原 schedules-state.json 只做
//! 一次性导入的来源，不再是权威）。触发前先 `claim` 占坑再执行，落定后
//! `confirm` —— 崩溃在两者之间，重启时按每作业的 `catch_up` 字段决定：
//! `catch_up=1`（晨报类）重跑一次 —— 至少一次；`catch_up=0`（通知类）
//! 直接放弃 —— 至多一次。**按作业类型给字段，不做全局二选一**。
//!
//! 每作业还有 `delivery`：`outbox`（默认，写文件）| `notify`（文件 +
//! 通知器，人不在屏幕前也该知道）| `quiet`（只落日志，高频任务不刷盘）。
//! 空回答（或模型答 `NO_REPLY`）跳过投递 —— 没内容的产出不值得打扰。
//!
//! 停机期间的错过：启动时回看自上次触发以来的分钟，**补跑最新一次**
//! （停了三天也只补一次，不补一串，回看封顶七天）。与 deepseek-harness
//! 的 schedule-resume 同口径：错过的不静默吞掉。

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use chrono::{Datelike, Local, NaiveDateTime, Timelike};
use joyczl_app_server::{run_turn, EventSink, Frame, Server};
use joyczl_protocol::{RequestId, ServerNotification, TurnStartParams};

/// 产出往哪送。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// 写 `<home>/outbox/`（默认 —— 定时任务的产出是可以慢慢读的文件）。
    Outbox,
    /// 写文件 **并** 推通知器（Slack/ntfy）—— 屏幕外也知道跑完了。
    Notify,
    /// 只落日志：高频任务（每 15 分钟盯一次 CI）不该刷出一堆文件。
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

/// 一条定时任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub name: String,
    /// 五字段 cron：`分 时 日 月 周`。
    pub cron: String,
    /// 触发时发给 Joy 的那句话。
    pub prompt: String,
    /// 错过/中断后要不要补跑。默认 true（至少一次 —— 晨报类宁可重不可漏）。
    pub catch_up: bool,
    /// 产出投递方式。默认 Outbox。
    pub delivery: Delivery,
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

/// 旧状态文件（M4b 之前的权威，现在的**一次性导入来源**）。读坏了就当
/// 没有 —— 它本来也只承载去重信息。
fn load_legacy_state(home: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(home.join("schedules-state.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 自 `from` 到 `to`（含两端，按分钟扫）有没有错过的触发点。返回**最后**
/// 那个错过点 —— 停机三天只补一次，不补一串。
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

/// 常驻循环：每 20 秒看一眼现在该不该触发。技能改动每次 tick 重新扫，
/// 等于热加载。
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

    // 登记 + 一次性导入 + 崩溃恢复 + 补跑，全在起跑线前做完。
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

        // 一次性导入：DB 没历史而旧 JSON 有 —— 搬一行进来，JSON 从此不再被读。
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

        // 崩溃恢复：claimed 未 confirm = 上次执行到一半进程没了。
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

        // 常规补跑：上次落定之后错过的最新触发点。
        if let Some(last) = schedules
            .last_fired(&job.name)
            .await
            .ok()
            .flatten()
            .and_then(|text| NaiveDateTime::parse_from_str(&text, "%Y-%m-%dT%H:%M").ok())
        {
            fired.insert(job.name.clone(), last.format("%Y-%m-%dT%H:%M").to_string());
            // 回看窗口封顶七天：再早的错过没有补的价值，也无谓地扫上百万分钟。
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
            // dispatch 先占坑再执行：崩溃在两者之间时，重启的恢复逻辑
            // 显式接管（补跑或放弃），而不是靠运气不重放。
            let _ = schedules.claim(&job.name, &minute).await;
            fired.insert(job.name.clone(), minute.clone());
            // 串行执行：同一分钟的其它任务等它跑完。定时任务打爆模型
            // 不是「快」，是没人读得过来。
            if let Err(e) = fire(&server, job, at).await {
                eprintln!("(joy) 任务 '{}' 执行失败：{e}", job.name);
            }
            // 成功失败都落定：没有退避机制，不该每 20 秒重试一次失败任务。
            let _ = schedules.confirm(&job.name, &minute).await;
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

    // 空回答（或模型按约定答 NO_REPLY）不值得投递 —— 写一个空文件、推一条
    // 空通知都是打扰。落一行日志就够。
    let trimmed = reply.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("no_reply") {
        eprintln!("(joy) 任务 '{}' 完成，但没有产出内容，跳过投递", job.name);
        return Ok(());
    }

    let home = server.settings().home;
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

    // notify 投递：屏幕外的人也该知道 —— 旁路，没配通知器或发送失败都不拦。
    if job.delivery == Delivery::Notify {
        if let Some(notifier) = server.notifier() {
            notifier.notify(joyczl_tools::notify::NotifyEvent::ScheduleRan {
                name: job.name.clone(),
                status: "完成".to_string(),
                preview: trimmed.chars().take(120).collect(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod schedule_tests;
