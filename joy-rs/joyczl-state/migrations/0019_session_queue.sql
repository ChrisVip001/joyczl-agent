-- 0.16: messages that run **after** the turn already in flight.
--
-- Why a table and not a Vec in the server: a queued instruction is a promise to the person who
-- typed it. Held in process memory it would mean "I queued it, Joy restarted, and it was
-- silently gone". Rows drain in id order, one turn at a time, and they survive a restart.
--
-- No status column: the row exists only while it has not been handed to a turn (claim-then-run,
-- see `Queue::pop`). History lives in chat_log once it actually runs.
CREATE TABLE IF NOT EXISTS session_queue (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    message    TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- The only query shape: oldest first, for one session.
CREATE INDEX IF NOT EXISTS session_queue_fifo ON session_queue (session_id, id);
