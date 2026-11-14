-- Legacy completed rows remain untouched; only new admitted batches can be recovered.
ALTER TABLE chat_log ADD COLUMN consolidation_batch TEXT;
ALTER TABLE chat_log ADD COLUMN consolidation_claimed_at TEXT;
CREATE INDEX chat_log_consolidation_batches ON chat_log(consolidated,consolidation_batch,consolidation_next_at);
CREATE TABLE memory_episode_contexts (
    context_digest TEXT PRIMARY KEY,
    source_refs_json TEXT NOT NULL,
    episode_id INTEGER REFERENCES episodes(id) ON DELETE SET NULL
);
