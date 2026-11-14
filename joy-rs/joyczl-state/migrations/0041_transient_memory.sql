-- Existing validity windows were explicitly set by a human.
ALTER TABLE memory_validity ADD COLUMN origin TEXT NOT NULL DEFAULT 'human'
    CHECK(origin IN ('human','consolidation'));
CREATE TABLE memory_transient_assessments (
    fact_id INTEGER NOT NULL,
    context_digest TEXT NOT NULL,
    assessment_json TEXT NOT NULL,
    PRIMARY KEY(fact_id,context_digest),
    FOREIGN KEY(fact_id,context_digest) REFERENCES memory_observations(fact_id,context_digest) ON DELETE CASCADE
);
