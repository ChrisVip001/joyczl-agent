-- Explicit turn lookup is also used when run and turn identifiers differ.
CREATE INDEX idx_task_runs_turn ON task_runs(turn_id);
