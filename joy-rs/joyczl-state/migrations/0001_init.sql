-- Joy 的 state.db：语义记忆（facts）、情景记忆（episodes）、原始对话日志。
-- 检索走 FTS5：零依赖的关键词检索，是「默认永远可用」的那一层（升级路径是
-- pgvector，见 README）。表与 FTS 索引靠三个触发器保持同步。

CREATE TABLE IF NOT EXISTS facts (
    id         INTEGER PRIMARY KEY,
    subject    TEXT NOT NULL,          -- 这条事实关于谁/什么，如 'alex'
    content    TEXT NOT NULL,          -- 事实本身
    source     TEXT NOT NULL DEFAULT 'user',  -- 'user' 直接告诉它 / 'consolidation' 提炼
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE VIRTUAL TABLE IF NOT EXISTS facts_fts USING fts5(
    subject, content, content='facts', content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS facts_ai AFTER INSERT ON facts BEGIN
    INSERT INTO facts_fts(rowid, subject, content) VALUES (new.id, new.subject, new.content);
END;
CREATE TRIGGER IF NOT EXISTS facts_ad AFTER DELETE ON facts BEGIN
    INSERT INTO facts_fts(facts_fts, rowid, subject, content) VALUES ('delete', old.id, old.subject, old.content);
END;
CREATE TRIGGER IF NOT EXISTS facts_au AFTER UPDATE ON facts BEGIN
    INSERT INTO facts_fts(facts_fts, rowid, subject, content) VALUES ('delete', old.id, old.subject, old.content);
    INSERT INTO facts_fts(rowid, subject, content) VALUES (new.id, new.subject, new.content);
END;

CREATE TABLE IF NOT EXISTS episodes (
    id          INTEGER PRIMARY KEY,
    happened_at TEXT NOT NULL,         -- ISO 8601 日期
    summary     TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE VIRTUAL TABLE IF NOT EXISTS episodes_fts USING fts5(
    summary, content='episodes', content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS episodes_ai AFTER INSERT ON episodes BEGIN
    INSERT INTO episodes_fts(rowid, summary) VALUES (new.id, new.summary);
END;
CREATE TRIGGER IF NOT EXISTS episodes_ad AFTER DELETE ON episodes BEGIN
    INSERT INTO episodes_fts(episodes_fts, rowid, summary) VALUES ('delete', old.id, old.summary);
END;

-- 原始对话日志。consolidation 从这里读未提炼的行；session_id 只是个标签，
-- 所以「新建会话」不是建表，换一个标签就行。
CREATE TABLE IF NOT EXISTS chat_log (
    id           INTEGER PRIMARY KEY,
    role         TEXT NOT NULL,        -- 'user' | 'assistant'
    content      TEXT NOT NULL,
    consolidated INTEGER NOT NULL DEFAULT 0,
    session_id   TEXT NOT NULL DEFAULT 'default',
    source       TEXT NOT NULL DEFAULT 'cli',  -- cli / voice / telegram / dashboard
    meta         TEXT,                 -- 一轮的遥测 JSON（gate/graph/iterations/…）
    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS chat_log_session_idx ON chat_log (session_id);
CREATE INDEX IF NOT EXISTS chat_log_consolidated_idx ON chat_log (consolidated);
