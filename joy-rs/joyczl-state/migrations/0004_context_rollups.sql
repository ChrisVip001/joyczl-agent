-- 上下文滚动摘要：滑窗之外的老对话被压成一段话，key 是会话。
--
-- covered_turns 记录「这段摘要覆盖了最早的多少轮」—— 只往前滚，不回头：
-- 每次滑窗新挤出若干轮，就把它们折进现有摘要，而不是重算全史（重算的
-- 成本随会话长度线性增长，而摘要的意义正是别让成本这样长）。
CREATE TABLE IF NOT EXISTS context_rollups (
    session_id    TEXT PRIMARY KEY,
    covered_turns INTEGER NOT NULL,
    summary       TEXT NOT NULL,
    updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
);
