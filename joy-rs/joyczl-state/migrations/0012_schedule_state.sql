-- M4b：调度状态进 SQLite（原先在 <home>/schedules-state.json 的 fired 表）。
--
-- 一张表管三件事：
--   1. 去重（last_fired_minute：同一任务同一分钟只触发一次 —— 原 fired 表的语义）；
--   2. 崩溃恢复（claimed_minute：已派发未落定 —— 重启时按 catch_up 决定补跑或放弃，
--      这是 openhuman「dispatch 先推进 next_run」防重放的 joy 版）；
--   3. 每作业投递策略（catch_up / delivery —— 晨报类保至少一次，通知类至多一次，
--      按作业类型给字段，不做全局二选一）。
--
-- 旧 JSON 的一次性导入不写在这：调度器启动时发现 DB 行没有 last_fired 而
-- JSON 里有，就搬一行进来（幂等 —— 第二次启动 DB 已有值，不再看 JSON）。
-- JSON 文件保留不删，只是从此不再被读。

CREATE TABLE IF NOT EXISTS schedule_state (
    name TEXT PRIMARY KEY,
    last_fired_minute TEXT,
    claimed_minute TEXT,
    -- 1 = 至少一次（错过的补跑一次）；0 = 至多一次（直接放弃，如实告知 stderr）。
    catch_up INTEGER NOT NULL DEFAULT 1,
    -- outbox（默认，写文件）| notify（文件 + 通知器）| quiet（只落盘日志）。
    delivery TEXT NOT NULL DEFAULT 'outbox',
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
