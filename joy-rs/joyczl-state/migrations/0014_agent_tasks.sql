-- 0014：长任务（agent_tasks）——目标是会死的，任务是会活的。
--
-- 目标（goal）此前只是进程内存里的一个字符串：重启即丢、无从谈起「暂停」。
-- 这张表把它升成实体：一个任务有身份、有名、有状态、有轮次账，跨进程活着。
-- 目标循环（judge 多轮）从「驱动者」降为「执行器」——它推的还是回合，但状态写这里。
--
-- status: pending（已创建未开跑）/ running（有回合在推）/ suspended（人按了暂停，
-- 或进程重启时正在跑——**挂起是人类开关，不自动续跑**）/ done / failed / cancelled。
--   * done：judge 判定达成，或单回合任务的那一轮正常结束。
--   * failed：judge 判定做不到 / 轮次上限 / 回合报错 —— 诚实记为没做成，不伪造完成。
--   * cancelled：人取消了。
--
-- 一会话一活跃任务：下方部分唯一索引兜底（pending/running/suspended 三态互斥）。
-- current_turn_id 是心跳：指着的回合一结束就清空；若进程在回合中途死掉，
-- turn_checkpoints（0013）里那条 open 会先被续跑，续上了任务回到 running。
--
-- 与 jobs（0009）的关系：jobs 是任务**在工具层**的子集——任务的一轮里可以起后台作业，
-- 但作业台账不挂在任务上（作业是事实，任务是意图；两者不互相记账）。
CREATE TABLE IF NOT EXISTS agent_tasks (
    id             TEXT PRIMARY KEY,
    session_id     TEXT NOT NULL,
    title          TEXT NOT NULL,
    goal_condition TEXT,
    status         TEXT NOT NULL DEFAULT 'pending',
    rounds_used    INTEGER NOT NULL DEFAULT 0,
    max_rounds     INTEGER NOT NULL DEFAULT 5,
    current_turn_id TEXT,
    suspend_reason TEXT,
    created_at     TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at     TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at    TEXT
);

-- 一会话最多一个非终态任务：数据库兜底，代码里的检查只是让报错更好看。
CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_tasks_one_active
    ON agent_tasks(session_id)
    WHERE status IN ('pending', 'running', 'suspended');

CREATE INDEX IF NOT EXISTS idx_agent_tasks_session_created
    ON agent_tasks(session_id, created_at DESC);
