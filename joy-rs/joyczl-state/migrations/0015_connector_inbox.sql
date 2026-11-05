-- 0015：连接器收件箱（入站）。
--
-- 出站已经有「草稿 → 人点头 → 发送」三道栅栏；入站是反方向：外面的东西**自己**
-- 想进来。所以这里只做最保守的一件事——把邮件原样记下来，等人决定要不要处理它。
--
-- 唯一键 (connector, mailbox, uid)：IMAP 的 UID 在同一个邮箱里单调递增，按它去重
-- 就能「重启不重复」。代价写在文档里：邮箱被重建后 UID 会从头开始，那时旧索引会
-- 挡住同 UID 的新邮件——判断依据是 message_id，不是 UID。
--
-- seen：人（或人设的 turn 常设授权）处理过没有。处理细节在回合自己的 trace 与
-- 对话日志里，这张表不重复记「哪次回合」——那个 id 在两条处理路径上都拿不可靠。
--
-- body 只存解出来的纯文本（text/plain 优先，text/html 去标签兜底）；附件不落盘——
-- 收进来的是「有人给你写了什么」，不是「有人给你发了什么文件」。
CREATE TABLE IF NOT EXISTS connector_inbox (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    connector   TEXT NOT NULL,
    mailbox     TEXT NOT NULL DEFAULT 'INBOX',
    uid         INTEGER NOT NULL,
    message_id  TEXT,
    from_addr   TEXT NOT NULL,
    subject     TEXT NOT NULL,
    received_at TEXT NOT NULL,
    snippet     TEXT NOT NULL,
    body        TEXT NOT NULL,
    seen        INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (connector, mailbox, uid)
);

CREATE INDEX IF NOT EXISTS idx_connector_inbox_unseen
    ON connector_inbox(connector, seen, received_at DESC);

-- 轮询游标：跟着「上次见过的最大 UID」走，而不是跟着「表里有什么」走。
-- 首次运行只记水位、不拉历史——收件箱里已有的几百封不是「新来的」，不该在第一次
-- 启动时全倒进台账。游标与台账分开，也让「清空收件箱」不会倒带游标。
CREATE TABLE IF NOT EXISTS connector_cursors (
    connector  TEXT NOT NULL,
    mailbox    TEXT NOT NULL,
    last_uid   INTEGER NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (connector, mailbox)
);
