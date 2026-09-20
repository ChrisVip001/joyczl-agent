-- 本地日历：create_event 工具的落点，list_events 的读取来源之一。
--
-- 同一场会议插第二遍会被数据库拒绝 —— 「三重预定」这种事故在 SQL 层就死了。
CREATE TABLE IF NOT EXISTS calendar_events (
    id        INTEGER PRIMARY KEY,
    title     TEXT NOT NULL,
    start     TEXT NOT NULL,             -- ISO 8601，分钟精度（2026-07-14T09:00）
    "end"     TEXT NOT NULL,             -- 同上；默认 start + 1h
    attendees TEXT NOT NULL DEFAULT '',
    notes     TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS calendar_events_title_start_idx
    ON calendar_events (title, start);
CREATE INDEX IF NOT EXISTS calendar_events_start_idx ON calendar_events (start);
