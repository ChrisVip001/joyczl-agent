-- M5 (0.14.0): a task can declare which model tier it runs on — "the task picks the model".
-- NULL = no declaration: the per-turn decision stands (auto mode) or the active profile does (fixed).
ALTER TABLE agent_tasks ADD COLUMN tier TEXT;
