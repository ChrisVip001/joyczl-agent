//! 混合检索：关键词那条腿 + 向量那条腿，用 RRF 合起来。
//!
//! 单靠关键词，问「上次说的那个上线安排」永远找不到「10 月 15 日发版」——
//! 字面上没有一个词重合。单靠向量，专名（`JOY_HOME`、`state.db`）又常常
//! 被糊掉。两条腿各自排序、再按名次融合，正好互补。
//!
//! **融合用名次不用分数**（RRF：`Σ 1/(k + rank)`，k=60）：关键词的 bm25 与
//! 余弦相似度量纲完全不同，硬凑成一个分数是编数据；名次是两者都有的、
//! 可比的东西。k=60 是原论文里的默认值，压住头部波动。
//!
//! 失败开放，与这个 crate 的其它成员一致：embedding 服务不可用就只用
//! 关键词那条腿，绝不让一次网络故障变成「什么都想不起来」。

use anyhow::Result;
use joyczl_provider::embed::Embedder;
use joyczl_state::{FactRow, Facts};

/// 向量那条腿的相似度门槛。低于它的当作无关 —— 余弦相似度在 0.3 以下
/// 基本是「同一个语言」而不是「同一个话题」。
const MIN_SIMILARITY: f32 = 0.30;

/// RRF 的名次平滑常数（原论文默认）。
const RRF_K: f32 = 60.0;

/// 余弦相似度。长度不等或零向量都返回 0（当作无关，而不是报错）。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// 两路结果的 RRF 融合。同名次并列时，先出现的（关键词那条腿）在前 ——
/// 精确匹配总是比「大概相关」更值得相信。
pub fn rrf_fuse(keyword: &[FactRow], semantic: &[FactRow]) -> Vec<FactRow> {
    use std::collections::HashMap;

    let mut scores: HashMap<i64, f32> = HashMap::new();
    let mut rows: HashMap<i64, FactRow> = HashMap::new();
    for (rank, row) in keyword.iter().enumerate() {
        *scores.entry(row.id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
        rows.entry(row.id).or_insert_with(|| row.clone());
    }
    for (rank, row) in semantic.iter().enumerate() {
        *scores.entry(row.id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
        rows.entry(row.id).or_insert_with(|| row.clone());
    }

    let mut ranked: Vec<(i64, f32)> = scores.into_iter().collect();
    // 分数降序；同分按 id 升序，保证结果稳定（同一个查询每次得到同样的顺序）。
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    ranked
        .into_iter()
        .filter_map(|(id, _)| rows.remove(&id))
        .collect()
}

/// 向量那条腿：算查询向量，跟库里所有带向量的行比一遍。
///
/// 出错就返回空 —— 调用方（`search_hybrid`）会退回纯关键词。
async fn semantic_hits(
    facts: &Facts,
    embedder: &Embedder,
    query: &str,
    top_k: usize,
) -> Result<Vec<FactRow>> {
    let query_vector = embedder.embed(query).await?;
    let mut scored: Vec<(f32, FactRow)> = facts
        .all_with_embedding()
        .await?
        .into_iter()
        .filter_map(|(row, vector)| {
            let score = cosine(&query_vector, &vector);
            (score >= MIN_SIMILARITY).then_some((score, row))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.id.cmp(&b.1.id))
    });
    Ok(scored.into_iter().take(top_k).map(|(_, row)| row).collect())
}

/// 混合检索。没给 embedder（开关关着）就是纯关键词 —— 与升级前完全一样。
pub async fn search_hybrid(
    facts: &Facts,
    embedder: Option<&Embedder>,
    query: &str,
    top_k: u32,
) -> Vec<FactRow> {
    let keyword = facts.search(query, top_k).await.unwrap_or_default();
    let Some(embedder) = embedder else {
        return keyword;
    };
    let semantic = match semantic_hits(facts, embedder, query, top_k as usize * 2).await {
        Ok(hits) => hits,
        Err(e) => {
            // 一条腿断了不致命：关键词那条腿照样站着。
            eprintln!("(joy) 向量检索不可用，这次只用关键词：{e}");
            return keyword;
        }
    };
    if semantic.is_empty() {
        return keyword;
    }
    rrf_fuse(&keyword, &semantic)
        .into_iter()
        .take(top_k as usize)
        .collect()
}

/// `joy memory reindex` 的执行体：给还没有向量的事实补上向量。
/// 返回补了多少条。
pub async fn reindex(facts: &Facts, embedder: &Embedder) -> Result<usize> {
    let mut done = 0usize;
    loop {
        let batch = facts.missing_embedding(50).await?;
        if batch.is_empty() {
            return Ok(done);
        }
        for row in batch {
            // subject + content 一起 embed：主题词常常正是检索时想问的词。
            let text = format!("{}: {}", row.subject, row.content);
            match embedder.embed(&text).await {
                Ok(vector) => {
                    facts.set_embedding(row.id, &vector).await?;
                    done += 1;
                }
                Err(e) => {
                    anyhow::bail!("第 {} 条算不出来（{e}）—— 已补 {done} 条", row.id);
                }
            }
        }
    }
}
