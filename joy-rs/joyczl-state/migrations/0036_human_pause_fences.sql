-- A checkpoint write cannot revoke a human pause, even when it arrives late.
CREATE TABLE task_pause_fences (
    task_id TEXT PRIMARY KEY REFERENCES agent_tasks(id) ON DELETE CASCADE,
    paused_at TEXT NOT NULL DEFAULT(datetime('now'))
);
-- Existing explicit pauses remain stopped. Known engine waits and restart suspensions are distinct.
INSERT INTO task_pause_fences(task_id)
SELECT id FROM agent_tasks WHERE status='suspended'
    AND COALESCE(suspend_reason,'') NOT IN (
        'waiting for background jobs',
        'waiting for authorized external input',
        'the process restarted; an unfinished turn resumes automatically if it can'
    );
