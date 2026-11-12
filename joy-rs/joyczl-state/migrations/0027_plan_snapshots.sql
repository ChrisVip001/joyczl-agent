-- A whole plan is a recovery unit; partial step rows are not.
CREATE TABLE task_plan_snapshots (
    task_id TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    steps_json TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (task_id, version)
);
