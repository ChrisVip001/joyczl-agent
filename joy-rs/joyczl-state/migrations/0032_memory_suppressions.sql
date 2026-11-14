-- Digests block re-extraction without keeping another plaintext copy of deleted claims.
CREATE TABLE memory_suppressions (
    key TEXT PRIMARY KEY,
    scope TEXT NOT NULL CHECK(scope IN ('fact','subject')),
    reason TEXT NOT NULL CHECK(reason IN ('deleted','corrected','forgotten_subject')),
    replacement_id INTEGER,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
