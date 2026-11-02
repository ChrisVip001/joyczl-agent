-- 用**实测**校准本地估算。
--
-- 每一次请求都有两个数字：provider 回报的 `usage.input_tokens`（实测的 prefill，
-- 权威）与我们本地按 tiktoken 估的输入量（近似）。两者的比值就是这台机器 + 这家
-- 厂商 + 这个模型的「估算偏差」，存下来给下一轮用 —— 估算偏小会让压缩来得太晚，
-- 偏大又会让它来得太早，而实测值本来就在手里，没有理由不用。
--
-- 每一轮覆盖写一行（只关心最近一次：模型/工具集变了，旧的比值就不作数了）。
CREATE TABLE IF NOT EXISTS session_context (
    session_id TEXT PRIMARY KEY,
    observed_input_tokens INTEGER NOT NULL,
    estimated_input_tokens INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);
