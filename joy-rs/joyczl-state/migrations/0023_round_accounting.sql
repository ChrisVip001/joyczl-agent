-- A model-loop round keeps its identity across interruption and recovery.
ALTER TABLE turn_checkpoints ADD COLUMN round_id TEXT;
CREATE TABLE run_round_accounts (
    run_id TEXT NOT NULL REFERENCES task_runs(id) ON DELETE CASCADE,
    round_id TEXT NOT NULL,
    tokens_in INTEGER NOT NULL DEFAULT 0,
    tokens_out INTEGER NOT NULL DEFAULT 0,
    tools_ok INTEGER NOT NULL DEFAULT 0,
    tools_failed INTEGER NOT NULL DEFAULT 0,
    wall_ms INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, round_id)
);
