-- Keep accepted instructions until their outcome has been recorded.
ALTER TABLE session_queue ADD COLUMN status TEXT NOT NULL DEFAULT 'pending';
CREATE UNIQUE INDEX session_queue_one_claim ON session_queue(session_id) WHERE status = 'claimed';
