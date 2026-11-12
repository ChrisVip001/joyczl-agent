//! Worker tests: it must work off the turn's clock, and it must stop when asked.
//!
//! The model is never called here — with fewer rows than the threshold, distillation is
//! not due, which is exactly the branch that makes "the worker runs every 60 s" cheap.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use joyczl_config::Settings;
use joyczl_provider::mock::Mock;
use joyczl_provider::Resolved;

use crate::memory_worker::{Deps, MemoryWorker};

async fn fixtures(
    name: &str,
) -> (
    Deps,
    std::path::PathBuf,
    Arc<std::sync::RwLock<Option<Resolved>>>,
) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().join(name);
    std::fs::create_dir_all(&home).expect("建目录");
    let _ = dir.keep();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");

    let settings = Settings {
        home: home.clone(),
        ..Settings::default()
    };
    let resolved = Arc::new(std::sync::RwLock::new(Some(Resolved {
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        vision: false,
        provider_id: "mock".to_string(),
        client: Arc::new(Mock::new(Vec::new())),
        model: "test-model".to_string(),
        small_model: "test-small".to_string(),
    })));

    let deps = Deps {
        chat: joyczl_state::Chat::new(pool.clone()),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool),
        settings: Arc::new(std::sync::RwLock::new(settings)),
        resolved: resolved.clone(),
    };
    (deps, home, resolved)
}

/// 后台一轮：把事实镜像进明文库，然后安静地停掉。
#[tokio::test]
async fn the_worker_mirrors_and_then_stops() {
    let (deps, home, _resolved) = fixtures("worker_mirror").await;
    deps.facts
        .add("alex", "喜欢早会", "user", "user")
        .await
        .expect("写事实");

    let worker = MemoryWorker::spawn(deps);
    worker.wake();
    // 轮询而不是固定 sleep：这是「最终一致」而不是「精确时钟」的断言。
    let page = home.join("memory").join("alex.md");
    let mut waited = 0;
    while !page.exists() && waited < 50 {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        waited += 1;
    }
    assert!(page.exists(), "镜像该把页面写出来：{}", page.display());

    worker.stop().await;
    // 停完之后不再干活：删掉页面再等心跳，它不该自己复活。
    std::fs::remove_file(&page).expect("删页");
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    assert!(!page.exists(), "停掉的 worker 不该再写文件");
}

/// 没有模型时不空转报错：直接跳过，pending 保持 0。
#[tokio::test]
async fn without_a_model_the_worker_stays_quiet() {
    let (deps, _home, resolved) = fixtures("worker_no_model").await;
    *resolved.write().expect("锁") = None;
    deps.facts
        .add("alex", "一句话", "user", "user")
        .await
        .expect("写");

    let worker = MemoryWorker::spawn(deps);
    worker.wake();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(worker.take_pending(), 0, "没模型就没有新事实可说");
    worker.stop().await;
}

/// pending 是「还没告诉客户端的事实数」：取一次就清零，不会重复播报。
#[test]
fn pending_facts_are_taken_once() {
    let worker = MemoryWorker {
        stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        wake: Arc::new(tokio::sync::Notify::new()),
        pending: Arc::new(std::sync::atomic::AtomicI32::new(3)),
        handle: std::sync::Mutex::new(None),
    };
    assert_eq!(worker.take_pending(), 3);
    assert_eq!(worker.take_pending(), 0, "同一批只播报一次");
    assert_eq!(worker.pending.load(Ordering::SeqCst), 0);
}
