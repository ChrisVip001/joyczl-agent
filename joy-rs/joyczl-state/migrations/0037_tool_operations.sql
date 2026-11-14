CREATE TABLE tool_operations (
    id TEXT PRIMARY KEY,
    task_id TEXT REFERENCES agent_tasks(id),
    run_id TEXT REFERENCES task_runs(id),
    turn_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    round_id TEXT NOT NULL,
    tool_use_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    input_json TEXT NOT NULL,
    input_digest TEXT NOT NULL,
    origin TEXT NOT NULL DEFAULT 'checkpoint' CHECK(origin IN ('checkpoint','recovered_checkpoint')),
    status TEXT NOT NULL DEFAULT 'intent' CHECK(status IN ('intent','succeeded','failed','reconciled_success','reconciled_failure','not_executed')),
    output_digest TEXT,
    evidence TEXT,
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    settled_at TEXT,
    UNIQUE(turn_id,round_id,tool_use_id)
);
CREATE INDEX tool_operations_task ON tool_operations(task_id,started_at);
