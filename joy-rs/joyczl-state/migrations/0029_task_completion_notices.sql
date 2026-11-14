ALTER TABLE agent_tasks ADD COLUMN completion_notify_requested INTEGER NOT NULL DEFAULT 0;
ALTER TABLE agent_tasks ADD COLUMN completion_notify_target TEXT;

CREATE TABLE task_completions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL,
    run_id TEXT,
    session_id TEXT,
    title TEXT NOT NULL,
    is_goal INTEGER NOT NULL,
    status TEXT NOT NULL,
    reason TEXT NOT NULL,
    notify_requested INTEGER NOT NULL,
    notify_target TEXT,
    created_at TEXT NOT NULL DEFAULT(datetime('now')),
    enqueued_at TEXT
);
CREATE INDEX task_completions_pending ON task_completions(notify_requested,enqueued_at);

-- The status change and production receipt succeed or roll back together, on every write path.
CREATE TRIGGER task_completion_committed AFTER UPDATE OF status ON agent_tasks
WHEN NEW.status<>OLD.status AND NEW.status IN ('done','failed','cancelled','suspended','needs_attention')
BEGIN
    INSERT INTO task_completions(task_id,run_id,session_id,title,is_goal,status,reason,notify_requested,notify_target)
    VALUES(NEW.id,COALESCE(OLD.current_turn_id,NEW.current_turn_id),NEW.session_id,NEW.title,
        NEW.goal_condition IS NOT NULL,
        CASE WHEN NEW.status='done' AND NEW.acceptance_json='[]' AND NEW.goal_condition IS NULL THEN 'produced_unverified' ELSE NEW.status END,
        CASE
            WHEN NEW.status='done' THEN COALESCE(NEW.last_stop_reason,'Task output produced; inspect its acceptance evidence')
            WHEN NEW.status='failed' THEN COALESCE(NEW.last_stop_reason,'Task failed; inspect the execution record')
            WHEN NEW.status='cancelled' THEN 'Task cancelled by the user'
            ELSE COALESCE(NEW.suspend_reason,NEW.last_stop_reason,'Task stopped for review')
        END,
        NEW.completion_notify_requested,NEW.completion_notify_target);
END;
