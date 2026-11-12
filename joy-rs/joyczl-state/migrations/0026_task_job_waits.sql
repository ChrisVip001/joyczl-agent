CREATE TABLE task_job_waits (
    task_id TEXT PRIMARY KEY REFERENCES agent_tasks(id) ON DELETE CASCADE,
    run_id TEXT NOT NULL REFERENCES task_runs(id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'waiting',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE task_job_wait_members (
    task_id TEXT NOT NULL REFERENCES task_job_waits(task_id) ON DELETE CASCADE,
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, job_id)
);
