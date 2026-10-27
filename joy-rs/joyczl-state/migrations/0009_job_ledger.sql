-- 0009：后台作业的台账（7x24 的地基）。
--
-- 作业此前只活在内存里：进程一死，「跑过什么、跑到哪、为什么没跑完」全部
-- 蒸发。这张表记的是**事实**，不是调度：崩溃后的策略是「标记 interrupted，
-- 不自动重跑」（dsh / hermes / pi 五家参考实现的一致选择）—— 自动重跑是
-- 事故放大器，重跑的决定留给人或模型，依据就是这里的记录。
--
-- log_path 指向 `<home>/jobs/<pid>-<id>.log`：作业输出随写随落的文件。
-- 带 pid 是因为 job id 是进程内自增的（j1、j2…），两个进程会撞名。
CREATE TABLE IF NOT EXISTS jobs (
    id          TEXT PRIMARY KEY,
    owner       TEXT NOT NULL,
    command     TEXT NOT NULL,
    status      TEXT NOT NULL,
    exit_code   INTEGER,
    log_path    TEXT,
    started_at  TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_jobs_owner_started ON jobs(owner, started_at DESC);
