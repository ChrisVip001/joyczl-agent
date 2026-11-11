-- 0017：任务从「会话的附属」升成「独立实体」。
--
-- 0014 的任务有三个先天限制：必须挂在会话上、一会话只能有一个、完成与否全靠 judge
-- 一张嘴。这一版把它们全部拆掉：
--
--   * session_id 可空 —— 任务可以不属于任何对话；
--   * 去掉「一会话一活跃任务」的部分唯一索引 —— 同一件事可以分多次执行；
--   * 加验收条件（acceptance_json）、预算（budget_json）、花费（spent_*）、工作目录（cwd）
--     与上次停留原因 —— 「完成」由可核对的事实决定，超限停在哪里也说得清。
--
-- 允许破坏性迁移：旧任务行不迁移（任务本身是短命的意图，结构变了就该重建）。
-- 旧的部分唯一索引随表一起消失，这是本迁移的目的之一，不是副作用。
--
-- 围绕任务的四张表的分工：
--   * task_runs       一次执行 = 一轮「开跑→收尾」，跨轮持久（谁在跑、跑了几次、花了多少）；
--   * task_steps      执行步骤 = 一次工具调用（P0），或计划里的一步（P1）。带 idempotency_key，
--                     重启对账只把 running 转 waiting，绝不重放有副作用的动作；
--   * task_events     事件流水（验收结果、预算超限、矛盾检出），人读时间线用；
--   * task_artifacts  交付物清单：这个任务到底产出了哪些文件。
--
-- status 加 needs_attention：验收没过、预算超了、副作用结果没落盘 —— 这些都是
-- 「需要人看一眼」，既不是 running（自己还在动）也不是 failed（做不成）。
-- CHECK 约束是兜底：状态字符串写错了当场失败，而不是等面板显示一个不认识的词。

DROP INDEX IF EXISTS idx_agent_tasks_one_active;
DROP INDEX IF EXISTS idx_agent_tasks_session_created;
DROP TABLE IF EXISTS agent_tasks;

CREATE TABLE agent_tasks (
    id               TEXT PRIMARY KEY,
    -- NULL = 这个任务不挂在任何会话上（任务独立存在）。
    session_id       TEXT,
    title            TEXT NOT NULL,
    -- 目标条件：有它才有多轮循环；NULL = 单轮任务。
    goal_condition   TEXT,
    -- 验收条件：`[{"kind":"fileExists","path":"report.md"}, …]`，空数组 = 没有客观条件。
    acceptance_json  TEXT NOT NULL DEFAULT '[]',
    -- 最近一次验收的逐条结果：`[{"label":…,"passed":false,"detail":"report.md 不存在"}]`。
    -- 存在行上而不是只写事件流水：任务卡要一次读全，不必再查事件表。
    checks_json      TEXT NOT NULL DEFAULT '[]',
    -- 预算：`{"wallMs":…,"tokens":…,"tools":…}`，缺的键 = 这一维不设上限。
    budget_json      TEXT NOT NULL DEFAULT '{}',
    -- 已花费：每轮结束累加；人看得到「用了多少」，判定也读它。
    spent_wall_ms    INTEGER NOT NULL DEFAULT 0,
    spent_tokens     INTEGER NOT NULL DEFAULT 0,
    spent_tools      INTEGER NOT NULL DEFAULT 0,
    -- 任务自己的工作目录：验收检查与写入都以它为边界；NULL = 没选目录，路径检查一律拒绝。
    cwd              TEXT,
    status           TEXT NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending', 'running', 'suspended', 'needs_attention',
                                       'done', 'failed', 'cancelled')),
    rounds_used      INTEGER NOT NULL DEFAULT 0,
    max_rounds       INTEGER NOT NULL DEFAULT 5,
    tier             TEXT,
    current_turn_id  TEXT,
    suspend_reason   TEXT,
    -- 上一次停下的原因（验收未通过 / 超预算 / 判断器故障…），任务卡脚注直接读它。
    last_stop_reason TEXT,
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at      TEXT
);

CREATE INDEX idx_agent_tasks_session_created ON agent_tasks(session_id, created_at DESC);
CREATE INDEX idx_agent_tasks_status ON agent_tasks(status, created_at ASC);

-- 一次执行。一个任务可以有多次执行（人点了继续、暂停后续跑、重启后重开）。
-- status: running / waiting（副作用结果没落盘，等人确认）/ completed / failed / cancelled。
CREATE TABLE task_runs (
    id           TEXT PRIMARY KEY,
    task_id      TEXT NOT NULL,
    session_id   TEXT,
    turn_id      TEXT,
    status       TEXT NOT NULL DEFAULT 'running',
    rounds       INTEGER NOT NULL DEFAULT 0,
    tokens_in    INTEGER NOT NULL DEFAULT 0,
    tokens_out   INTEGER NOT NULL DEFAULT 0,
    tools_ok     INTEGER NOT NULL DEFAULT 0,
    tools_failed INTEGER NOT NULL DEFAULT 0,
    stop_reason  TEXT,
    started_at   TEXT NOT NULL DEFAULT (datetime('now')),
    ended_at     TEXT
);

CREATE INDEX idx_task_runs_task ON task_runs(task_id, started_at DESC);
CREATE INDEX idx_task_runs_status ON task_runs(status, started_at ASC);

-- 执行步骤。P0 的粒度 = 一次工具调用（kind='tool'，tool 是工具名，input_json 是参数）；
-- P1 引入 Planner 后只增加 kind='plan'，结构不动。
-- idempotency_key = `<turn_id>:<seq>`：同一个回合的同一步只有一条。
-- status: running（已发未落盘）/ ok / failed / waiting（需要人确认）/ skipped。
CREATE TABLE IF NOT EXISTS task_steps (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id         TEXT NOT NULL,
    run_id          TEXT,
    seq             INTEGER NOT NULL,
    kind            TEXT NOT NULL DEFAULT 'tool',
    tool            TEXT,
    input_json      TEXT,
    status          TEXT NOT NULL DEFAULT 'running',
    note            TEXT,
    idempotency_key TEXT,
    output_digest   TEXT,
    started_at      TEXT NOT NULL DEFAULT (datetime('now')),
    ended_at        TEXT
);

CREATE UNIQUE INDEX idx_task_steps_key ON task_steps(idempotency_key)
    WHERE idempotency_key IS NOT NULL;
CREATE INDEX idx_task_steps_run ON task_steps(run_id, seq);
CREATE INDEX idx_task_steps_task ON task_steps(task_id, id);

-- 执行事件流水：验收逐条结果、预算超限、工具自述与事实不符 —— 人读的时间线。
CREATE TABLE IF NOT EXISTS task_events (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id  TEXT NOT NULL,
    run_id   TEXT,
    seq      INTEGER NOT NULL DEFAULT 0,
    kind     TEXT NOT NULL,
    detail   TEXT,
    at       TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_task_events_task ON task_events(task_id, id);

-- 交付物：执行过程中写下的文件，同一路径只留一条（重复写更新大小与时间）。
CREATE TABLE IF NOT EXISTS task_artifacts (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL,
    run_id  TEXT,
    path    TEXT NOT NULL,
    bytes   INTEGER NOT NULL DEFAULT 0,
    at      TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(task_id, path)
);

CREATE INDEX idx_task_artifacts_task ON task_artifacts(task_id, id);

-- checkpoint 挂到 Run 上：恢复时知道「续的是哪一次执行」，也便于对账。
-- plan_version 留给 P1 的 Planner（计划改版会换版本号），P0 恒为 NULL。
ALTER TABLE turn_checkpoints ADD COLUMN run_id TEXT;
ALTER TABLE turn_checkpoints ADD COLUMN step_seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE turn_checkpoints ADD COLUMN plan_version INTEGER;

-- 对话日志挂到任务上：回看一个任务说过什么，不必靠解析 meta。
ALTER TABLE chat_log ADD COLUMN task_id TEXT;
CREATE INDEX IF NOT EXISTS chat_log_task_idx ON chat_log (task_id);
