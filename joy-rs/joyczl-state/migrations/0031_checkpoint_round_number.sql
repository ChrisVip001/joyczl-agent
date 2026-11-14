-- Admit the round number atomically with its first execution checkpoint.
ALTER TABLE turn_checkpoints ADD COLUMN round_no INTEGER CHECK(round_no > 0);
