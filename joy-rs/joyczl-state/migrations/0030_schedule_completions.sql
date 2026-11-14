CREATE TABLE schedule_completions (
    name TEXT NOT NULL,
    minute TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('produced','empty','needs_attention','reviewed')),
    detail TEXT NOT NULL,
    output TEXT NOT NULL,
    preview TEXT NOT NULL,
    output_path TEXT,
    output_written_at TEXT,
    notify_requested INTEGER NOT NULL,
    notify_target TEXT,
    created_at TEXT NOT NULL DEFAULT(datetime('now')),
    enqueued_at TEXT,
    reviewed_at TEXT,
    PRIMARY KEY(name,minute)
);
CREATE INDEX schedule_completions_pending ON schedule_completions(notify_requested,enqueued_at);
