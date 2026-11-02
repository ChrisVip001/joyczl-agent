//! 融合那部分的测试：余弦、RRF 的名次合并、以及「开关关着时与从前完全一样」。
//!
//! 网络那一侧（真的去调 embedding 服务）不在这里测 —— 它失败开放的契约
//! 由 `search_hybrid` 的 embedder 参数与 eprintln 保证，而真正的 HTTP
//! 只在配了 embedding 模型时才发生。

use joyczl_state::FactRow;

use super::retrieval::{cosine, rrf_fuse, search_hybrid};

fn row(id: i64, content: &str) -> FactRow {
    FactRow {
        id,
        subject: format!("s{id}"),
        content: content.to_string(),
        source: "test".to_string(),
        kind: "fact".to_string(),
        created_at: None,
    }
}

#[test]
fn cosine_is_the_cosine() {
    assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    assert!((cosine(&[1.0, 1.0], &[1.0, 1.0]) - 1.0).abs() < 1e-6);
    // 退化输入当作无关，而不是报错或 NaN。
    assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
    assert_eq!(cosine(&[], &[]), 0.0);
    assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
}

#[test]
fn rrf_favours_documents_both_legs_agree_on() {
    // 2 号两条腿都命中；1 号只有关键词，3 号只有向量。
    let keyword = vec![row(1, "a"), row(2, "b")];
    let semantic = vec![row(2, "b"), row(3, "c")];

    let fused = rrf_fuse(&keyword, &semantic);
    let ids: Vec<i64> = fused.iter().map(|r| r.id).collect();
    assert_eq!(ids[0], 2, "两条腿都命中的排第一：{ids:?}");
    assert_eq!(ids.len(), 3, "两边的并集，不重不漏");
    assert_eq!(fused[0].content, "b", "行内容要原样带回来");
}

#[test]
fn rrf_is_stable_across_runs() {
    // 同分时按 id 升序：同一个查询每次都得到同样的顺序。
    let keyword = vec![row(7, "x")];
    let semantic = vec![row(3, "y")];
    let first: Vec<i64> = rrf_fuse(&keyword, &semantic).iter().map(|r| r.id).collect();
    let second: Vec<i64> = rrf_fuse(&keyword, &semantic).iter().map(|r| r.id).collect();
    assert_eq!(first, second);

    // 只有一条腿时，也是那条腿自己的顺序。
    let only_keyword = rrf_fuse(&keyword, &[]);
    assert_eq!(only_keyword[0].id, 7);
}

#[tokio::test]
async fn without_an_embedder_retrieval_is_exactly_keyword() {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite 还要写 -wal/-shm：目录不能在这里被删掉
    let facts = joyczl_state::Facts::new(pool);

    facts
        .add("alex", "prefers morning meetings", "user", "user")
        .await
        .unwrap();
    facts
        .add("bob", "ships on Fridays", "user", "user")
        .await
        .unwrap();

    let hybrid = search_hybrid(&facts, None, "morning", 4).await;
    let keyword = facts.search("morning", 4).await.unwrap();
    let hybrid_ids: Vec<i64> = hybrid.iter().map(|r| r.id).collect();
    let keyword_ids: Vec<i64> = keyword.iter().map(|r| r.id).collect();
    assert_eq!(hybrid_ids, keyword_ids, "开关关着 = 与升级前完全一样");
}
