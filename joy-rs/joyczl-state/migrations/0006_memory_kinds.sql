-- 记忆类型学 + 提炼重试账。
--
-- 1) `facts.kind`：一条事实**关于什么**。有了它，「用户说过的话」「别人给的反馈」
--    「项目的现状」「参考资料」能被分开取用 —— 不然检索只能按词面碰运气。
--    默认 `fact`：老行、以及分类不出来的，都落在这里，不会丢。
ALTER TABLE facts ADD COLUMN kind TEXT NOT NULL DEFAULT 'fact';

-- 2) 提炼失败的重试账。从前失败一律「下次再来」，于是同一批坏行每轮都被重试
--    一次（白烧模型调用）。现在失败会记次数并按指数退避排到未来某个时刻。
--    成功仍然是 `consolidated = 1`：两套语义并行，互不冲突。
ALTER TABLE chat_log ADD COLUMN consolidation_tries INTEGER NOT NULL DEFAULT 0;
ALTER TABLE chat_log ADD COLUMN consolidation_next_at TEXT;
