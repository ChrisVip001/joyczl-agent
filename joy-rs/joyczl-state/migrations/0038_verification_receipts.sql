CREATE TABLE task_verifications (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES agent_tasks(id),
    run_id TEXT NOT NULL REFERENCES task_runs(id),
    round_id TEXT NOT NULL,
    acceptance_json TEXT NOT NULL,
    checks_json TEXT NOT NULL,
    files_json TEXT NOT NULL,
    previous_id TEXT REFERENCES task_verifications(id),
    measured_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX task_verifications_latest ON task_verifications(task_id,run_id,measured_at);
ALTER TABLE task_completions ADD COLUMN verification_id TEXT REFERENCES task_verifications(id);
CREATE TRIGGER task_completion_verification AFTER INSERT ON task_completions
WHEN NEW.status='done'
BEGIN
    UPDATE task_completions SET verification_id=(SELECT id FROM task_verifications
        WHERE task_id=NEW.task_id AND run_id=NEW.run_id
        AND acceptance_json=(SELECT acceptance_json FROM agent_tasks WHERE id=NEW.task_id)
        AND checks_json=(SELECT checks_json FROM agent_tasks WHERE id=NEW.task_id) ORDER BY rowid DESC LIMIT 1)
        WHERE id=NEW.id;
END;

-- A model's goal judgement is a stopping decision, not independent acceptance evidence.
CREATE TRIGGER task_completion_unverified_goal AFTER INSERT ON task_completions
WHEN NEW.status='done' AND EXISTS(SELECT 1 FROM agent_tasks WHERE id=NEW.task_id AND acceptance_json='[]')
BEGIN
    UPDATE task_completions SET status='produced_unverified',verification_id=NULL WHERE id=NEW.id;
END;
