-- 记忆写入去重。
--
-- 起因：提炼（consolidation）每 N 轮跑一次，模型重复 `save_note` 也是常事。没有
-- 去重时同一句话会一次次进库，检索结果里出现十条一模一样的「Alex 喜欢早上的会议」
-- —— 既浪费上下文，也让「这条有多可信」没法从重复次数上读出来（重复不等于更多
-- 证据，只是我们没查重）。
--
-- 键取 (subject, 规范化 content)：大小写与首尾/内部空白不算区别（模型排版会有出入），
-- 别的都算。**先清历史、再立规** —— 唯一索引建不起来的话，后面每次写入都会报错。

-- 1) 历史重复：保留最早的那条（id 最小 = 先记的）。
DELETE FROM facts
WHERE id NOT IN (
    SELECT MIN(id) FROM facts GROUP BY subject, lower(trim(content))
);

-- 2) 从此一条 (subject, 规范化 content) 只留一条。
--    表达式索引，SQLite 认（lower/trim 都是确定性函数）。
CREATE UNIQUE INDEX IF NOT EXISTS facts_subject_content_unique
    ON facts (subject, lower(trim(content)));
