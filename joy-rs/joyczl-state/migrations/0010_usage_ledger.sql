-- 0010：用量台账（M2d，只记账不强制）。
--
-- usage.jsonl（trace 层）是追加日志，查不了「这个模型一共花了多少」。
-- 这张表是**可查询的聚合底座**：每 turn 一行，per-model 汇总用 SQL 现算。
--
-- 刻意没有成本列：BYOK 模式下钱记在用户自己的厂商账单上，费率表是部署方
-- 的事（ token × 费率随时可从这张表算出来）。五家参考实现里 hermes 记了
-- estimated_cost_usd —— 等平台供 key（M4）再补，现在填的是拍脑袋数字。
CREATE TABLE IF NOT EXISTS usage_ledger (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id    TEXT NOT NULL,
    provider      TEXT NOT NULL,
    model         TEXT NOT NULL,
    input_tokens  INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_usage_model ON usage_ledger(model, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_usage_session ON usage_ledger(session_id, created_at DESC);
