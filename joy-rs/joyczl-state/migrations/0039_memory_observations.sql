-- Derivation context is traceability, not independent proof of a claim.
CREATE TABLE memory_observations (
    fact_id INTEGER NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
    context_digest TEXT NOT NULL,
    source_refs_json TEXT NOT NULL,
    observed_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(fact_id, context_digest)
);
