//! `models.rs` 的纯逻辑：视图、provider 校验、当前标签。
//!
//! 需要 `Server` 的那部分（存/切/删）靠 `joyczl-config` 的 models 单测 +
//! 冒烟覆盖 —— 这里只钉住不依赖进程状态的三件事。

use super::*;

fn profile(id: &str, provider: &str, key: Option<&str>) -> cfg::ModelProfile {
    cfg::ModelProfile {
        id: id.to_string(),
        provider: provider.to_string(),
        model: Some("m1".to_string()),
        small_model: None,
        base_url: None,
        key_source: key.map(|k| k.to_string()),
    }
}

/// 视图里只有**凭据指针**，没有密钥本身 —— 界面要显示「key 存在哪」，
/// 不该把 key 端给浏览器。
#[test]
fn views_carry_the_key_pointer_but_never_the_secret() {
    let mut file = cfg::ModelsFile::default();
    cfg::upsert(&mut file, profile("ds", "deepseek", Some("keychain")));
    cfg::upsert(
        &mut file,
        profile("oa", "openai", Some("env:OPENAI_API_KEY")),
    );
    file.active = Some("ds".to_string());

    let out = views(&file);
    assert_eq!(out.len(), 2);
    assert!(out[0].active, "ds 是当前那套");
    assert!(!out[1].active);
    assert_eq!(out[0].key_source.as_deref(), Some("keychain"));
    assert_eq!(out[1].key_source.as_deref(), Some("env:OPENAI_API_KEY"));
    let serialized = serde_json::to_string(&out).expect("序列化");
    assert!(!serialized.contains("sk-"), "视图不该带密钥：{serialized}");
}

/// provider 校验：表内的放行；表外的要有 base_url，否则报出可用的清单。
#[test]
fn provider_validation_accepts_known_ids_and_self_hosted_endpoints() {
    let known = joyczl_provider::PROVIDERS.first().expect("表里至少一家").id;
    assert!(validate_provider(known, None).is_ok());

    let err = validate_provider("不存在的厂", None).unwrap_err();
    assert!(err.contains("不认识的 provider"), "{err}");
    assert!(err.contains(known), "报错该列出可用的：{err}");

    // 自建端点：provider 只决定用哪种 wire 格式。
    assert!(validate_provider("不存在的厂", Some("http://localhost:8000/v1")).is_ok());
}

/// 「当前是哪个模型」的一句话：有模型就带上，没有就只说厂商。
#[test]
fn the_current_label_names_provider_and_model() {
    let mut settings = joyczl_config::Settings {
        provider: "deepseek".to_string(),
        ..joyczl_config::Settings::default()
    };
    assert_eq!(current_label(&settings).as_deref(), Some("deepseek"));

    settings.model = Some("deepseek-chat".to_string());
    assert_eq!(
        current_label(&settings).as_deref(),
        Some("deepseek / deepseek-chat")
    );

    let empty = joyczl_config::Settings {
        provider: String::new(),
        ..joyczl_config::Settings::default()
    };
    assert_eq!(current_label(&empty), None);
}

// ---- 端到端：真的存、真的切、真的删（临时 home，不碰系统钥匙串） ------------

async fn server() -> (Server, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let server = Server::boot(
        pool,
        joyczl_config::Settings {
            home,
            ..joyczl_config::Settings::default()
        },
    )
    .await;
    // 钉成文件后端：单测绝不碰系统钥匙串（macOS 的 security 会弹框，CI 里
    // 会直接挂住）。临时目录就是这次的「兜底文件」所在地。
    server
        .secrets
        .lock()
        .expect("secrets 锁不该中毒")
        .set_backend(joyczl_config::models::SecretBackend::File);
    (server, dir)
}

fn save(id: &str, provider: &str, model: Option<&str>) -> ModelSaveParams {
    ModelSaveParams {
        id: id.to_string(),
        provider: Some(provider.to_string()),
        model: model.map(|m| m.to_string()),
        small_model: None,
        base_url: None,
        api_key: None,
        key_env: Some("JOY_TEST_KEY".to_string()),
        make_active: None,
    }
}

/// 存两套 → 切换 → 删一套：active 一路跟得上，文件里没有密钥。
#[tokio::test]
async fn saving_switching_and_removing_end_to_end() {
    let (server, dir) = server().await;

    let first = server
        .model_save(save("ds", "deepseek", Some("deepseek-chat")))
        .expect("存第一套");
    assert!(first.active, "第一次存完就该是当前的");
    assert_eq!(first.key_stored_in, "env:JOY_TEST_KEY");
    assert_eq!(first.current.as_deref(), Some("deepseek / deepseek-chat"));

    server
        .model_save(save("oa", "openai", Some("gpt-5.2")))
        .expect("存第二套");
    let list = server.model_profiles();
    assert_eq!(list.active_id.as_deref(), Some("oa"), "后存的成了当前");
    assert_eq!(list.profiles.len(), 2);
    assert!(list.profiles.iter().all(|p| p.key_source.is_some()));

    // 只带 id + make_active = 切换，不用重发密钥。
    let switched = server
        .model_save(ModelSaveParams {
            provider: None,
            model: None,
            ..save("ds", "", None)
        })
        .expect("切换");
    assert!(switched.active);
    assert_eq!(
        server.model_profiles().active_id.as_deref(),
        Some("ds"),
        "切回去了"
    );
    // 切换没把配置洗掉：provider/model 还是原来那套。
    let back = server.model_profiles();
    let ds = back
        .profiles
        .iter()
        .find(|p| p.id == "ds")
        .expect("ds 还在");
    assert_eq!(ds.provider, "deepseek");
    assert_eq!(ds.model.as_deref(), Some("deepseek-chat"));

    // 删掉当前那套：active 跟着清空，不留悬空指针。
    let removed = server.model_remove(ModelRemoveParams {
        id: "ds".to_string(),
    });
    assert!(removed.removed);
    assert!(removed.active_id.is_none());

    // 落盘的文件里只有配置与指针，没有密钥本身。
    let raw = std::fs::read_to_string(dir.path().join("models.json")).expect("models.json");
    assert!(!raw.contains("sk-"), "文件里不该有密钥：{raw}");
    assert!(raw.contains("env:JOY_TEST_KEY"), "凭据指针该在：{raw}");
}

/// 不认识的 provider 直接拒绝，并把可用的清单报出来 —— 不是一句「参数不对」。
#[tokio::test]
async fn an_unknown_provider_is_refused_with_the_list() {
    let (server, _dir) = server().await;
    let err = server
        .model_save(save("x", "不存在的厂", None))
        .unwrap_err();
    assert!(err.contains("不认识的 provider"), "{err}");
    assert!(
        err.contains(joyczl_provider::PROVIDERS[0].id),
        "报错该列出可用的：{err}"
    );
    assert!(
        server.model_profiles().profiles.is_empty(),
        "什么都没存进去"
    );
}

/// 第一次配置不给 provider 也不行（切换时才可以省）。
#[tokio::test]
async fn a_new_profile_needs_a_provider() {
    let (server, _dir) = server().await;
    let err = server
        .model_save(ModelSaveParams {
            provider: None,
            ..save("裸", "", None)
        })
        .unwrap_err();
    assert!(err.contains("第一次配置"), "{err}");
}
