-- 会话账本：一轮里发生过的每一件事，结构化、只追加、可重建。
--
-- 为什么要有它：`chat_log` 是给人的**视图**（一句一轮、工具活动折叠成文本），
-- 助手回复落库前已经被 `fold_tool_activity` 压成 `[tools used: name(前200字符)]`，
-- 工具参数与结果原文从未入账。于是「从记录里重建这一轮」做不到，
-- 换更强的模型重新整理记忆也无从下手 —— 账本就是那个不可能再丢的源头。
--
-- 三条规矩：
--   1. 没有 UPDATE 路径。改正是新行，不是改旧行。
--   2. `fidelity` 声明保真度，绝不猜：回填自 chat_log 的行是 `folded`
--      （它们的工具输出在写入时就已经被截断，还原不了原文）。
--   3. 大内容转存到 `<home>/ledger/`，行里只留指针 + 摘要 + 字节数。

CREATE TABLE IF NOT EXISTS ledger (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    turn_id    TEXT NOT NULL,
    kind       TEXT NOT NULL,                 -- user | assistant | tool_use | tool_result | image | notice
    payload    TEXT NOT NULL,                 -- 结构化 JSON；转存时是 {ptr,digest,bytes,preview}
    digest     TEXT NOT NULL,                 -- sha256(canonical payload)
    fidelity   TEXT NOT NULL DEFAULT 'exact', -- exact | folded
    at         TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS ledger_turn_idx ON ledger (session_id, turn_id, seq);
CREATE INDEX IF NOT EXISTS ledger_session_idx ON ledger (session_id, seq);

-- 一轮一行：轮摘要、事件数、给人看的一句预览，以及「这一轮被整理过没有」的账。
-- 单独一张表是为了列会话时不必扫 payload；它同样是只追加的（consolidated 那几列
-- 是队列状态，不是内容）。
CREATE TABLE IF NOT EXISTS ledger_turns (
    session_id            TEXT NOT NULL,
    turn_id               TEXT NOT NULL,
    turn_digest           TEXT NOT NULL,        -- 有序行摘要的摘要：轮级篡改证据
    events                INTEGER NOT NULL,
    fidelity              TEXT NOT NULL DEFAULT 'exact',
    preview               TEXT NOT NULL DEFAULT '',
    at                    TEXT NOT NULL DEFAULT (datetime('now')),
    consolidated          INTEGER NOT NULL DEFAULT 0,
    consolidation_tries   INTEGER NOT NULL DEFAULT 0,
    consolidation_next_at TEXT,
    PRIMARY KEY (session_id, turn_id)
);

CREATE INDEX IF NOT EXISTS ledger_turns_pending ON ledger_turns (consolidated, at);

-- 事实的出处与新鲜度。明文记忆库（<home>/memory/）是权威，这几列是索引侧的
-- 旁注：这个事实对应哪一页、那页的指纹、最后一次被确认是什么时候。
ALTER TABLE facts ADD COLUMN page_path TEXT;
ALTER TABLE facts ADD COLUMN digest TEXT;
ALTER TABLE facts ADD COLUMN last_confirmed_at TEXT;
ALTER TABLE facts ADD COLUMN confirm_count INTEGER NOT NULL DEFAULT 0;
