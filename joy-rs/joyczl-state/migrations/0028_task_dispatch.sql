-- Human-authorized initial execution; one durable dispatch per task.
CREATE TABLE task_dispatch (
    task_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL UNIQUE,
    turn_id TEXT NOT NULL UNIQUE,
    message TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','claimed','finished','interrupted')),
    created_at TEXT NOT NULL DEFAULT(datetime('now')),
    finished_at TEXT
);
CREATE INDEX task_dispatch_pending ON task_dispatch(state, created_at);
