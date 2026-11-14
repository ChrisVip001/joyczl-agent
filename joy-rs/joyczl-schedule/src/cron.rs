//! Five-field cron: one matcher, three callers.
//!
//! `joy schedule` asks it every minute "is this job due now"; the capability center asks it
//! "when does this run next", so the dashboard can show a time instead of a pattern. Those
//! two questions must agree — a second copy would drift the first time one side learned a
//! syntax the other did not, and the drift would look like "the panel says 08:00, nothing
//! ran at 08:00".
//!
//! Supported: `*`, `*/step`, `a-b` ranges, `a,b` lists, five fields
//! (minute hour day month weekday). Sunday is `0`, and `7` is normalized to it. No seconds,
//! no names (`MON`), no `@daily` — the parser is hand-written so a single unsupported token
//! is a `false` rather than a silently shifted schedule.

use chrono::{Datelike, Duration, NaiveDateTime, Timelike};

/// How far ahead `next_after` will look. A yearly job fits; a cron that can never match
/// (February 30th) costs one honest `None` instead of an infinite scan.
const MAX_LOOKAHEAD_MINUTES: i64 = 400 * 24 * 60;

/// Does `cron` fire at `at` (minute resolution)?
pub fn matches(cron: &str, at: NaiveDateTime) -> bool {
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

/// The first minute **strictly after** `from` that fires, or `None` when the pattern is
/// malformed or cannot match within [`MAX_LOOKAHEAD_MINUTES`].
///
/// Minute-by-minute rather than field arithmetic: a correct field-skipping implementation
/// is four special cases deep (month lengths, weekday rollover, leap day), and this runs
/// once per panel refresh for a handful of jobs. The common case — something due today —
/// exits within 1440 steps.
pub fn next_after(cron: &str, from: NaiveDateTime) -> Option<NaiveDateTime> {
    if !valid(cron) {
        return None;
    }
    let mut at = from + Duration::minutes(1);
    for _ in 0..MAX_LOOKAHEAD_MINUTES {
        if matches(cron, at) {
            return Some(at);
        }
        at += Duration::minutes(1);
    }
    None
}

/// The next fire as `YYYY-MM-DD HH:MM`, or the sentence explaining why there is none.
///
/// One formatter for the panel and for the model's answer: two would eventually disagree
/// about what "next" means the moment the clock crosses a minute.
pub fn next_run(cron: &str, from: NaiveDateTime) -> Result<String, String> {
    if !valid(cron) {
        return Err(
            "the cron expression is wrong (five fields: minute hour day month weekday)".to_string(),
        );
    }
    next_after(cron, from)
        .map(|at| at.format("%Y-%m-%d %H:%M").to_string())
        .ok_or_else(|| "this cron will not fire within a year".to_string())
}

/// Is this a well-formed five-field cron?
///
/// Structural only: `0 0 30 2 *` is *valid* and never fires. Rejecting it here would mean
/// pretending to know the calendar better than the human who wrote it; the panel shows
/// "there is no next run" instead, which says the same thing without guessing.
pub fn valid(cron: &str) -> bool {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    [
        (fields[0], 0, 59),
        (fields[1], 0, 23),
        (fields[2], 1, 31),
        (fields[3], 1, 12),
        (fields[4], 0, 7),
    ]
    .iter()
    .all(|(field, min, max)| field_valid(field, *min, *max))
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

/// Every segment must parse and sit inside its bound — a list is only as valid as its
/// worst element, because `field_matches` would happily ignore the bad half.
fn field_valid(field: &str, min: u32, max: u32) -> bool {
    if field == "*" {
        return true;
    }
    field.split(',').all(|part| {
        let part = part.trim();
        if let Some(step) = part.strip_prefix("*/") {
            return step.parse::<u32>().is_ok_and(|step| step > 0);
        }
        if let Some((from, to)) = part.split_once('-') {
            return match (from.trim().parse::<u32>(), to.trim().parse::<u32>()) {
                (Ok(from), Ok(to)) => from <= to && from >= min && to <= max,
                _ => false,
            };
        }
        part.parse::<u32>().is_ok_and(|n| n >= min && n <= max)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(y, mo, d)
            .expect("date")
            .and_hms_opt(h, mi, 0)
            .expect("time")
    }

    #[test]
    fn a_daily_eight_oclock_lands_on_the_next_eight_oclock() {
        let from = at(2026, 11, 12, 9, 30);
        assert_eq!(next_after("0 8 * * *", from), Some(at(2026, 11, 13, 8, 0)));
    }

    #[test]
    fn a_minute_that_already_matches_is_not_returned_again() {
        // Strictly after: asking at 08:00 must not answer 08:00, or a panel would show a
        // run that already happened as the next one.
        let from = at(2026, 11, 12, 8, 0);
        assert_eq!(next_after("0 8 * * *", from), Some(at(2026, 11, 13, 8, 0)));
    }

    #[test]
    fn weekdays_skip_the_weekend() {
        // 2026-11-13 is a Friday; the next weekday 08:00 is Monday the 16th.
        let from = at(2026, 11, 13, 9, 0);
        assert_eq!(
            next_after("0 8 * * 1-5", from),
            Some(at(2026, 11, 16, 8, 0))
        );
    }

    #[test]
    fn a_step_pattern_finds_the_next_quarter_hour() {
        let from = at(2026, 11, 12, 10, 7);
        assert_eq!(
            next_after("*/15 * * * *", from),
            Some(at(2026, 11, 12, 10, 15))
        );
    }

    #[test]
    fn malformed_patterns_are_rejected_instead_of_guessed_at() {
        for bad in [
            "",
            "0 8 * *",
            "0 8 * * * *",
            "0 25 * * *",
            "0 8 * * MON",
            "*/0 * * * *",
        ] {
            assert!(!valid(bad), "{bad} should not be accepted as valid");
            assert_eq!(next_after(bad, at(2026, 11, 12, 0, 0)), None, "{bad}");
        }
        assert!(valid("0 8 * * 1-5"));
        assert!(valid("0 8,20 * * *"));
        assert!(valid("*/15 * * * *"));
    }

    #[test]
    fn an_impossible_date_is_valid_but_has_no_next_run() {
        // Structural validity and "will it ever fire" are different questions; the panel
        // needs both answers separately.
        assert!(valid("0 0 30 2 *"));
        assert_eq!(next_after("0 0 30 2 *", at(2026, 11, 12, 0, 0)), None);
    }

    #[test]
    fn sunday_can_be_written_as_seven() {
        let sunday = at(2026, 11, 15, 8, 0);
        assert!(matches("0 8 * * 7", sunday));
        assert!(matches("0 8 * * 0", sunday));
    }
}
