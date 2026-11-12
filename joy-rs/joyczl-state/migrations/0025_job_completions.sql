ALTER TABLE jobs ADD COLUMN notify_requested INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN notify_target TEXT;
CREATE TABLE job_completions (
    job_id TEXT PRIMARY KEY REFERENCES jobs(id) ON DELETE CASCADE,
    notification_enqueued INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
