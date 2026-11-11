-- 0018：会话有了自己的一行（0.15）。
--
-- 之前「会话」只是 chat_log.session_id 上的一个标签：没有名字、没有工作目录，空会话根本
-- 进不了列表，标题只能靠第一条用户消息推导。这里给它一张真正的元数据表。
--
-- 本轮只落最小列（id / title / cwd / created_at / last_at）。owner、visibility、holder、
-- lease_until 这些属于多用户阶段，届时用 ALTER TABLE 加 —— 现在加只会得到一堆恒为同一个
-- 值的空列。
CREATE TABLE IF NOT EXISTS sessions (
    id         TEXT PRIMARY KEY,
    -- 人起的名字；NULL 时读取侧回退「第一条用户消息」，与 0018 之前的行为一致。
    title      TEXT,
    -- 本会话的工作目录：它同时也是这个会话唯一的可写范围根。NULL = 还没选。
    cwd        TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_at    TEXT
);
CREATE INDEX IF NOT EXISTS sessions_recent ON sessions(last_at DESC);

-- 旧会话反填：标题留空（回退第一条用户消息），时间取 chat_log 的聚合。
INSERT OR IGNORE INTO sessions (id, created_at, last_at)
SELECT session_id, MIN(created_at), MAX(created_at) FROM chat_log GROUP BY session_id;
