-- 0011：审批审计台账（M3b）。
--
-- 「问了才跑」的因果一直在协议里，但「模型问过什么、结果如何」之前只活在
-- 内存与 stderr —— turn 一结束就没了。这张表是审计记录：hub 管理员事后
-- 能回答「谁批准了 rm -rf」这类问题。
--
-- outcome 四态：approved（人批了）/ denied（人拒了）/ timeout（没人答，超时
-- 拒绝）/ voided（turn 结束，问题作废）。写入口只有一个 —— 批准请求的等待
-- future 收尾处，四个出口都从那里过。
CREATE TABLE IF NOT EXISTS approval_log (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    turn_id      TEXT NOT NULL,
    request_id   TEXT NOT NULL,
    tool         TEXT NOT NULL,
    args_preview TEXT NOT NULL,
    reason       TEXT NOT NULL,
    outcome      TEXT NOT NULL,
    decided_at   TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_approval_log_turn ON approval_log(turn_id, decided_at DESC);
