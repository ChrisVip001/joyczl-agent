-- Bind new production receipts to the exact run, without rewriting historical receipts.
DROP TRIGGER task_completion_committed;
CREATE TRIGGER task_completion_committed AFTER UPDATE OF status ON agent_tasks
WHEN NEW.status<>OLD.status AND NEW.status IN ('done','failed','cancelled','suspended','needs_attention')
BEGIN
    INSERT INTO task_completions(task_id,run_id,session_id,title,is_goal,status,reason,notify_requested,notify_target)
    VALUES(NEW.id,(SELECT CASE WHEN COUNT(*)=1 THEN MIN(id) END FROM task_runs
        WHERE task_id=NEW.id AND (turn_id=COALESCE(OLD.current_turn_id,NEW.current_turn_id)
        OR (turn_id IS NULL AND id=COALESCE(OLD.current_turn_id,NEW.current_turn_id)))),NEW.session_id,NEW.title,
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
