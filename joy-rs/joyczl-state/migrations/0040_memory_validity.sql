CREATE TABLE memory_validity (
    fact_id INTEGER PRIMARY KEY REFERENCES facts(id) ON DELETE CASCADE,
    valid_from TEXT,
    valid_until TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    CHECK(valid_from IS NULL OR julianday(valid_from) IS NOT NULL),
    CHECK(valid_until IS NULL OR julianday(valid_until) IS NOT NULL),
    CHECK(valid_from IS NULL OR valid_until IS NULL OR julianday(valid_from)<julianday(valid_until))
);
CREATE VIEW active_facts AS SELECT f.* FROM facts f LEFT JOIN memory_validity v ON v.fact_id=f.id
WHERE (v.valid_from IS NULL OR julianday(v.valid_from)<=julianday('now'))
AND (v.valid_until IS NULL OR julianday(v.valid_until)>julianday('now'));
-- Time-bounded claims stay out of static mirrors, which cannot expire themselves.
CREATE VIEW durable_facts AS SELECT f.* FROM active_facts f LEFT JOIN memory_validity v ON v.fact_id=f.id
WHERE v.valid_from IS NULL AND v.valid_until IS NULL;
