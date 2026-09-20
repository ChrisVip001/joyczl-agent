//! create_event / list_events —— 日历工具。
//!
//! 事件落在哪儿，输出里必须写清楚 —— 模型会把这段话转述给用户，
//! 所以 Joy 永远不会虚报「已同步到云端」：
//!   永远     state.db（确定性评测断言的位置）+ calendar.ics（可导入文件）
//!   opt-in   Apple Calendar（`JOY_APPLE_CALENDAR=1`，经 AppleScript）
//!   未实现   Google Calendar —— Joy 还没有 OAuth 连接流程，设了开关也只会
//!            如实说没同步。诚实的「没做」好过撒谎的「做了」。

use std::path::Path;
use std::sync::Arc;

use chrono::{Datelike, Duration as ChronoDuration, NaiveDateTime, Timelike};
use serde_json::{json, Value};

use crate::{opt_u32, require_str, Tool, ToolCtx};

/// 把 calendar.ics 追加一个最小 VEVENT。ISO 的 `2026-07-14T09:00` 变成
/// ICS 的紧凑形式 `20260714T090000`。
fn write_ics(
    home: &Path,
    title: &str,
    start: &str,
    end: &str,
    attendees: &str,
) -> std::io::Result<()> {
    let dt = |s: &str| {
        // 分钟精度（16 字符，如 2026-07-14T09:00）补上秒位。
        let mut compact = s.replace(['-', ':'], "");
        if s.chars().count() == 16 {
            compact.push_str("00");
        }
        compact
    };
    let event = format!(
        "BEGIN:VEVENT\nSUMMARY:{title}\nDTSTART:{dtstart}\nDTEND:{dtend}\nDESCRIPTION:attendees: {attendees}\nEND:VEVENT\n",
        dtstart = dt(start),
        dtend = dt(end),
    );
    let path = home.join("calendar.ics");
    let body = match std::fs::read_to_string(&path) {
        Ok(existing) => existing.replace("END:VCALENDAR\n", ""),
        Err(_) => "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//joyczl-agent//EN\n".to_string(),
    };
    std::fs::write(path, format!("{body}{event}END:VCALENDAR\n"))
}

fn parse_iso(value: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(&value[..value.len().min(16)], "%Y-%m-%dT%H:%M").ok()
}

/// Apple Calendar 的 AppleScript 日期要从 ISO 的**各部分**拼 —— 永远不喂
/// 格式化好的日期串（解析依赖系统 locale，是经典翻车点）。先把日设成 1
/// 再设月/年，避开「31 号设成 30 天的月份会滚到下个月」的溢出。
fn applescript_date(var: &str, at: NaiveDateTime) -> String {
    format!(
        "set {var} to current date\nset day of {var} to 1\n\
         set year of {var} to {year}\nset month of {var} to {month}\n\
         set day of {var} to {day}\nset hours of {var} to {hour}\n\
         set minutes of {var} to {minute}\nset seconds of {var} to 0\n",
        year = at.year(),
        month = at.month(),
        day = at.day(),
        hour = at.hour(),
        minute = at.minute(),
    )
}

const APPLE_CALENDAR_NAME: &str = "Joy";

/// 写进 Calendar.app（专用的 "Joy" 日历，首次使用时创建）。macOS 弹权限框
/// 会让 osascript 超时 —— 那时如实告诉用户：事件已在本地，批了再建一遍。
async fn sync_to_apple_calendar(title: &str, start: &str, end: &str, notes: &str) -> String {
    if !cfg!(target_os = "macos") {
        return "Apple Calendar 同步跳过（不是 macOS）。".to_string();
    }
    let (Some(start_at), Some(end_at)) = (parse_iso(start), parse_iso(end)) else {
        return "Apple Calendar 同步失败（时间格式解析不了）—— 事件仍在本地日历。".to_string();
    };
    // AppleScript 字符串里的引号必须清掉，双引号换成单引号即可。
    let safe = |s: &str| s.replace('\\', "").replace('"', "'");
    let script = format!(
        "{start}{end}\
tell application \"Calendar\"\n  \
if not (exists calendar \"{name}\") then\n    \
try\n      \
make new calendar with properties {{name:\"{name}\"}}\n      \
delay 1\n    \
end try\n  \
end if\n  \
if exists calendar \"{name}\" then\n    \
set targetCal to calendar \"{name}\"\n  \
else\n    \
set targetCal to first calendar whose writable is true\n  \
end if\n  \
tell targetCal\n    \
make new event with properties {{summary:\"{title}\", start date:startDate, end date:endDate, description:\"{notes}\"}}\n  \
end tell\n  \
return name of targetCal\n\
end tell",
        start = applescript_date("startDate", start_at),
        end = applescript_date("endDate", end_at),
        name = APPLE_CALENDAR_NAME,
        title = safe(title),
        notes = safe(notes),
    );

    let outcome = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .await;
    match outcome {
        Err(e) => format!("Apple Calendar 同步失败（{e}）—— 事件仍在本地日历。"),
        Ok(output) => {
            if output.status.success() {
                let used = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let used = if used.is_empty() {
                    APPLE_CALENDAR_NAME
                } else {
                    &used
                };
                format!("也已加进 Apple Calendar（日历「{used}」）。")
            } else {
                let detail = String::from_utf8_lossy(&output.stderr);
                let detail = detail.trim().chars().take(120).collect::<String>();
                format!(
                    "Apple Calendar 同步失败（{detail}）—— 事件仍在本地日历。\
                     若是权限问题，去 系统设置 > 隐私与安全性 > 自动化 里放行终端控制 Calendar。"
                )
            }
        }
    }
}

pub fn create_event() -> Tool {
    Tool {
        name: "create_event".to_string(),
        description: "在用户的本地日历上创建日程。用户想安排、预约、计划某个具体时间的\
                      事情时用。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "简短的事件标题"},
                "start": {"type": "string", "description": "开始时间，ISO 8601，如 2026-07-14T09:00"},
                "end": {"type": "string", "description": "结束时间，ISO 8601。缺省 = 开始 + 1 小时"},
                "attendees": {"type": "string", "description": "逗号分隔的参与者名字/邮箱"},
                "notes": {"type": "string", "description": "可选的事件备注"}
            },
            "required": ["title", "start"]
        }),
        handler: Arc::new(|ctx: ToolCtx, args: Value| {
            Box::pin(async move {
                let title = require_str(&args, "title")?;
                let start = require_str(&args, "start")?;
                let attendees = args
                    .get("attendees")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let notes = args
                    .get("notes")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();

                // 防御：模型有时会发半截调用。给一条能改的提示，而不是裸错误。
                let Some(start_at) = parse_iso(&start) else {
                    return Ok(
                        "create_event 至少需要 title 和 start（ISO 8601，如 2026-07-14T09:00）。\
                         请补全后重试。"
                            .to_string(),
                    );
                };
                // 统一到分钟精度：2026-07-11T17:00:00 → 2026-07-11T17:00。
                let start = start_at.format("%Y-%m-%dT%H:%M").to_string();
                let end = match args.get("end").and_then(Value::as_str) {
                    Some(e) if !e.trim().is_empty() => match parse_iso(e.trim()) {
                        Some(at) => at.format("%Y-%m-%dT%H:%M").to_string(),
                        None => {
                            return Ok(format!(
                                "end 解析不了：'{e}'。要 ISO 8601，如 2026-07-14T10:00。"
                            ))
                        }
                    },
                    _ => (start_at + ChronoDuration::hours(1))
                        .format("%Y-%m-%dT%H:%M")
                        .to_string(),
                };

                // add 的幂等性在 SQL 层：同标题 + 同开始时间只会有一条。
                let stored = ctx
                    .calendar
                    .add(&title, &start, &end, &attendees, &notes)
                    .await?;
                if stored.is_none() {
                    return Ok(format!(
                        "事件「{title}」（{start}）已经存在，没有重复创建。"
                    ));
                }
                write_ics(&ctx.home, &title, &start, &end, &attendees)
                    .map_err(|e| anyhow::anyhow!("写 calendar.ics 失败：{e}"))?;

                let mut where_landed = format!(
                    "已存进本地日历（{}）。",
                    ctx.home.join("calendar.ics").display()
                );
                if env_flag("JOY_APPLE_CALENDAR") {
                    where_landed.push(' ');
                    where_landed
                        .push_str(&sync_to_apple_calendar(&title, &start, &end, &notes).await);
                }
                if env_flag("JOY_GOOGLE_CALENDAR") {
                    where_landed.push_str(
                        " Google Calendar：Joy 还没实现 Google 的连接流程，这次没有同步 ——\
                         事件只在本地日历里。",
                    );
                }
                if !env_flag("JOY_APPLE_CALENDAR") && !env_flag("JOY_GOOGLE_CALENDAR") {
                    where_landed.push_str(
                        " 没有同步到任何日历应用（想开就设 JOY_APPLE_CALENDAR=1，\
                         或手动导入 calendar.ics）。",
                    );
                }
                let with = if attendees.is_empty() {
                    String::new()
                } else {
                    format!("，参与者 {attendees}")
                };
                Ok(format!(
                    "事件已创建：「{title}」{start} → {end}{with}。{where_landed}"
                ))
            })
        }),
    }
}

pub fn list_events() -> Tool {
    Tool {
        name: "list_events".to_string(),
        description: "读用户日历上 Joy 创建的事件。用户问某天/某周有什么安排时用。\
                      日期用 ISO（如 2026-07-10）；两个都不给就列出全部。\
                      「今天/昨天」这类词先用 current_time 换算成日期。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "start": {"type": "string", "description": "最早日期（含当日），ISO，如 2026-07-10"},
                "end": {"type": "string", "description": "最晚日期（含当日），ISO，如 2026-07-10"},
                "limit": {"type": "integer", "description": "最多几条，默认 20"}
            },
            "required": []
        }),
        handler: Arc::new(|ctx: ToolCtx, args: Value| {
            Box::pin(async move {
                let start = opt_str(&args, "start");
                let end = opt_str(&args, "end");
                let limit = opt_u32(&args, "limit", 20)?.clamp(1, 100);
                let rows = ctx
                    .calendar
                    .list(start.as_deref(), end.as_deref(), limit)
                    .await?;
                if rows.is_empty() {
                    let window = match (&start, &end) {
                        (Some(s), Some(e)) => format!("（{s} 到 {e}）"),
                        (Some(s), None) => format!("（{s} 起）"),
                        (None, Some(e)) => format!("（到 {e}）"),
                        (None, None) => String::new(),
                    };
                    return Ok(format!(
                        "没有找到事件{window}。查过：Joy 的本地日历\
                         （只包含 Joy 自己创建的事件，Calendar.app 里手动建的不在这里）。"
                    ));
                }
                let lines: Vec<String> = rows
                    .iter()
                    .map(|r| {
                        let who = if r.attendees.is_empty() {
                            String::new()
                        } else {
                            format!("，与 {}", r.attendees)
                        };
                        format!("- {}：{} → {}{}", r.title, r.start, r.end, who)
                    })
                    .collect();
                Ok(format!(
                    "来自 Joy 的本地日历（Joy 创建的事件）：\n{}",
                    lines.join("\n")
                ))
            })
        }),
    }
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}
