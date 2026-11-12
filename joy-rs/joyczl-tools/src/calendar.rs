//! create_event / list_events — calendar tools.
//!
//! Where an event lands must be spelled out in the output — the model relays this to the
//! user, so Joy can never falsely claim "synced to the cloud":
//!   always    state.db (where deterministic test assertions look) + calendar.ics (importable)
//!   opt-in    Apple Calendar (`JOY_APPLE_CALENDAR=1`, via AppleScript)
//!   not impl  Google Calendar — Joy has no OAuth connect flow yet; the flag would only
//!             honestly say "not synced". An honest "not done" beats a lying "done".

use std::path::Path;
use std::sync::Arc;

use chrono::{Datelike, Duration as ChronoDuration, NaiveDateTime, Timelike};
use serde_json::{json, Value};

use crate::{opt_u32, require_str, Tool, ToolCtx};

/// Append a minimal VEVENT to calendar.ics. ISO's `2026-07-14T09:00` becomes ICS's
/// compact form `20260714T090000`.
fn write_ics(
    home: &Path,
    title: &str,
    start: &str,
    end: &str,
    attendees: &str,
) -> std::io::Result<()> {
    let dt = |s: &str| {
        // Minute precision (16 chars, e.g. 2026-07-14T09:00) gets the seconds appended.
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

/// Apple Calendar's AppleScript date must be assembled from ISO's **parts** — never feed
/// a preformatted date string (parsing depends on system locale, a classic failure point).
/// Set the day to 1 first, then month/year, avoiding the "day 31 rolls into next month" overflow.
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

/// Writes into Calendar.app (a dedicated "Joy" calendar, created on first use). macOS's
/// permission dialog can time osascript out — then say honestly: it's local; approve and retry.
async fn sync_to_apple_calendar(title: &str, start: &str, end: &str, notes: &str) -> String {
    if !cfg!(target_os = "macos") {
        return "Apple Calendar sync skipped (not macOS).".to_string();
    }
    let (Some(start_at), Some(end_at)) = (parse_iso(start), parse_iso(end)) else {
        return "Apple Calendar sync failed (the time could not be parsed) — the event is still in the local calendar.".to_string();
    };
    // Quotes inside the AppleScript string must be stripped; double quotes become single ones.
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
        Err(e) => {
            format!("Apple Calendar sync failed ({e}) — the event is still in the local calendar.")
        }
        Ok(output) => {
            if output.status.success() {
                let used = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let used = if used.is_empty() {
                    APPLE_CALENDAR_NAME
                } else {
                    &used
                };
                format!("Also added to Apple Calendar (calendar \"{used}\").")
            } else {
                let detail = String::from_utf8_lossy(&output.stderr);
                let detail = detail.trim().chars().take(120).collect::<String>();
                format!(
                    "Apple Calendar sync failed ({detail}) — the event is still in the local calendar.\n                     If this is a permissions problem, allow the terminal to control Calendar under System Settings > Privacy & Security > Automation."
                )
            }
        }
    }
}

pub fn create_event() -> Tool {
    Tool {
        name: "create_event".to_string(),
        description: "Create an event on the user's local calendar. Use it when the user wants to schedule, book or plan something at a specific time."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "A short event title"},
                "start": {"type": "string", "description": "Start time, ISO 8601, e.g. 2026-07-14T09:00"},
                "end": {"type": "string", "description": "End time, ISO 8601. Default = start + 1 hour"},
                "attendees": {"type": "string", "description": "Comma-separated attendee names/addresses"},
                "notes": {"type": "string", "description": "An optional note for the event"}
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

                // Defense: models sometimes send half a call. Give an actionable hint, not a bare error.
                let Some(start_at) = parse_iso(&start) else {
                    return Ok(
                        "create_event needs at least title and start (ISO 8601, e.g. 2026-07-14T09:00). Please fill them in and retry."
                            .to_string(),
                    );
                };
                // Normalize to minute precision: 2026-07-11T17:00:00 → 2026-07-11T17:00.
                let start = start_at.format("%Y-%m-%dT%H:%M").to_string();
                let end = match args.get("end").and_then(Value::as_str) {
                    Some(e) if !e.trim().is_empty() => match parse_iso(e.trim()) {
                        Some(at) => at.format("%Y-%m-%dT%H:%M").to_string(),
                        None => {
                            return Ok(format!(
                                "end could not be parsed: '{e}'. It needs ISO 8601, e.g. 2026-07-14T10:00."
                            ))
                        }
                    },
                    _ => (start_at + ChronoDuration::hours(1))
                        .format("%Y-%m-%dT%H:%M")
                        .to_string(),
                };

                // add's idempotency lives in the SQL layer: same title + same start yields one row.
                let stored = ctx
                    .calendar
                    .add(&title, &start, &end, &attendees, &notes)
                    .await?;
                if stored.is_none() {
                    return Ok(format!(
                        "The event \"{title}\" ({start}) already exists; not creating a duplicate."
                    ));
                }
                write_ics(&ctx.home, &title, &start, &end, &attendees)
                    .map_err(|e| anyhow::anyhow!("Could not write calendar.ics: {e}"))?;

                let mut where_landed = format!(
                    "Saved to the local calendar ({}).",
                    ctx.home.join("calendar.ics").display()
                );
                if env_flag("JOY_APPLE_CALENDAR") {
                    where_landed.push(' ');
                    where_landed
                        .push_str(&sync_to_apple_calendar(&title, &start, &end, &notes).await);
                }
                if env_flag("JOY_GOOGLE_CALENDAR") {
                    where_landed.push_str(
                        " Google Calendar: Joy has no Google connection flow yet, so nothing was synced this time — the event lives only in the local calendar.",
                    );
                }
                if !env_flag("JOY_APPLE_CALENDAR") && !env_flag("JOY_GOOGLE_CALENDAR") {
                    where_landed.push_str(
                        " Nothing was synced to a calendar app (set JOY_APPLE_CALENDAR=1 to enable it, or import calendar.ics by hand).",
                    );
                }
                let with = if attendees.is_empty() {
                    String::new()
                } else {
                    format!(", attendees {attendees}")
                };
                Ok(format!(
                    "Event created: \"{title}\" {start} → {end}{with}.{where_landed}"
                ))
            })
        }),
    }
}

pub fn list_events() -> Tool {
    Tool {
        name: "list_events".to_string(),
        description: "Read the events Joy created on the user's calendar. Use it when the user asks what is scheduled for a day or a week. Dates are ISO (e.g. 2026-07-10); give neither to list everything. Convert \"today\"/\"yesterday\" with current_time first."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "start": {"type": "string", "description": "Earliest date (inclusive), ISO, e.g. 2026-07-10"},
                "end": {"type": "string", "description": "Latest date (inclusive), ISO, e.g. 2026-07-10"},
                "limit": {"type": "integer", "description": "How many at most, 20 by default"}
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
                        (Some(s), Some(e)) => format!("({s} to {e})"),
                        (Some(s), None) => format!("(from {s})"),
                        (None, Some(e)) => format!("(to {e})"),
                        (None, None) => String::new(),
                    };
                    return Ok(format!(
                        "No events found{window}. Looked at: Joy's local calendar (it holds only the events Joy itself created; anything made by hand in Calendar.app is not here)."
                    ));
                }
                let lines: Vec<String> = rows
                    .iter()
                    .map(|r| {
                        let who = if r.attendees.is_empty() {
                            String::new()
                        } else {
                            format!(", with {}", r.attendees)
                        };
                        format!("- {}：{} → {}{}", r.title, r.start, r.end, who)
                    })
                    .collect();
                Ok(format!(
                    "From Joy's local calendar (events Joy created):\n{}",
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
