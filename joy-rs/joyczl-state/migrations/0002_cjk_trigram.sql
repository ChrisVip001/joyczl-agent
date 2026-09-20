-- FTS5 默认用 unicode61 分词，它把一整串连续的中文当**一个词**：
--   存进去的是「阿明喜欢早上的会议」，查「早上」匹配不到。
-- 换成 trigram：按连续三字符切片建索引，中文天然可用，英文顺带也拿到了
-- 子串匹配（查 "demo" 能命中 "demos"）。代价是索引约 3 倍大 ——
-- 个人记忆库几千行，这个代价可以忽略。
--
-- 注意 trigram 的查询词至少要 3 个字符才能切出三元组；fts::to_match_expr
-- 不做长度过滤，短词只是匹配不到而已，不会报错。
--
-- 重建索引这步不能省：换了分词器，旧索引的格式已经不对了。

DROP TRIGGER IF EXISTS facts_ai;
DROP TRIGGER IF EXISTS facts_ad;
DROP TRIGGER IF EXISTS facts_au;
DROP TABLE IF EXISTS facts_fts;

CREATE VIRTUAL TABLE facts_fts USING fts5(
    subject, content, content='facts', content_rowid='id', tokenize='trigram'
);
CREATE TRIGGER facts_ai AFTER INSERT ON facts BEGIN
    INSERT INTO facts_fts(rowid, subject, content) VALUES (new.id, new.subject, new.content);
END;
CREATE TRIGGER facts_ad AFTER DELETE ON facts BEGIN
    INSERT INTO facts_fts(facts_fts, rowid, subject, content) VALUES ('delete', old.id, old.subject, old.content);
END;
CREATE TRIGGER facts_au AFTER UPDATE ON facts BEGIN
    INSERT INTO facts_fts(facts_fts, rowid, subject, content) VALUES ('delete', old.id, old.subject, old.content);
    INSERT INTO facts_fts(rowid, subject, content) VALUES (new.id, new.subject, new.content);
END;
INSERT INTO facts_fts(facts_fts) VALUES ('rebuild');

DROP TRIGGER IF EXISTS episodes_ai;
DROP TRIGGER IF EXISTS episodes_ad;
DROP TABLE IF EXISTS episodes_fts;

CREATE VIRTUAL TABLE episodes_fts USING fts5(
    summary, content='episodes', content_rowid='id', tokenize='trigram'
);
CREATE TRIGGER episodes_ai AFTER INSERT ON episodes BEGIN
    INSERT INTO episodes_fts(rowid, summary) VALUES (new.id, new.summary);
END;
CREATE TRIGGER episodes_ad AFTER DELETE ON episodes BEGIN
    INSERT INTO episodes_fts(episodes_fts, rowid, summary) VALUES ('delete', old.id, old.summary);
END;
INSERT INTO episodes_fts(episodes_fts) VALUES ('rebuild');
