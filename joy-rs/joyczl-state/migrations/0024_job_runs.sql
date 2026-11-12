ALTER TABLE jobs ADD COLUMN run_id TEXT;
ALTER TABLE jobs ADD COLUMN task_id TEXT;
CREATE INDEX jobs_run ON jobs(run_id);
CREATE INDEX jobs_task ON jobs(task_id);
