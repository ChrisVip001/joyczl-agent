-- Input obligations are declared by humans; events cannot create or expand task authority.
CREATE TABLE task_input_conditions (
    condition_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL UNIQUE,
    source TEXT NOT NULL,
    event_key TEXT NOT NULL,
    description TEXT NOT NULL,
    expires_at TEXT,
    event_id TEXT,
    payload_json TEXT,
    received_at TEXT,
    consumed_run_id TEXT,
    created_at TEXT NOT NULL DEFAULT(datetime('now'))
);
CREATE TABLE task_input_events (
    source TEXT NOT NULL,
    event_id TEXT NOT NULL,
    condition_id TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    received_at TEXT NOT NULL DEFAULT(datetime('now')),
    PRIMARY KEY(source,event_id)
);
CREATE TABLE task_input_waits (
    task_id TEXT PRIMARY KEY,
    condition_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'waiting' CHECK(state IN ('waiting','claimed','cancelled')),
    created_at TEXT NOT NULL DEFAULT(datetime('now'))
);
CREATE INDEX task_input_waits_pending ON task_input_waits(state,created_at);
