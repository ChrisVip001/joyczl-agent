-- 0013：回合级 checkpoint（长任务引擎的地基）。
--
-- 回合此前是「用完即弃」的临时物：模型 thoughts、工具结果只活在进程内存里，
-- 进程一死（崩溃、hub 重启、闲置驱逐），整个回合从头再来——已跑过的工具全部重跑。
-- 这张表把回合的工作记忆逐边界落盘：每个工具结果回合结束写一次 checkpoint，
-- 崩溃后对账发现 status='open' 的行，从已完成的迭代继续，绝不重跑已完成的工具。
--
-- 与 jobs 台账（0009）的分界：jobs 记**事实**（跑过什么、结果如何），对账只标记
-- 不重跑；turn_checkpoints 存的是**可续跑的状态**（messages_json 就能在新进程里
-- 接着推理），对账即自动续跑——用户的回合是用户亲口交代的，续上它是本分。
-- status: open（可续跑）/ done（回合正常结束）/ abandoned（被用户打断，不自动续跑）。
--
-- usage_json 是崩溃前的 token 消耗：续跑后的回合要把两段账加在一起，不能丢了前半段。
CREATE TABLE IF NOT EXISTS turn_checkpoints (
    turn_id      TEXT PRIMARY KEY,
    session_id   TEXT NOT NULL,
    user_message TEXT NOT NULL,
    messages_json TEXT NOT NULL,
    iterations   INTEGER NOT NULL DEFAULT 0,
    usage_json   TEXT NOT NULL DEFAULT '{}',
    status       TEXT NOT NULL DEFAULT 'open',
    updated_at   TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_turn_checkpoints_status ON turn_checkpoints(status, updated_at DESC);
