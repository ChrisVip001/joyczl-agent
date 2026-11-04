//! Pure logic of `models.rs`: views, provider validation, the current label.
//!
//! The part that needs `Server` (save/switch/remove) is covered by joyczl-config's models unit tests
//! plus smoke coverage — this file only pins the three things that do not depend on process state.

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

/// The view carries only the **credential pointer**, never the key itself — the UI shows "where the key
/// is stored" and must not hand the key to the browser.
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

/// Provider validation: in-catalog ids pass; out-of-catalog ones need a base_url, otherwise the error lists what is available.
#[test]
fn provider_validation_accepts_known_ids_and_self_hosted_endpoints() {
    let catalog = joyczl_provider::catalog::Catalog::built_in();
    let known = catalog.ids().first().copied().expect("目录里至少一家");
    assert!(validate_provider(&catalog, known, None).is_ok());

    let err = validate_provider(&catalog, "不存在的厂", None).unwrap_err();
    assert!(err.contains("不认识的 provider"), "{err}");
    assert!(err.contains(known), "报错该列出可用的：{err}");

    // Self-hosted endpoint: the provider only decides which wire format is used.
    assert!(validate_provider(&catalog, "不存在的厂", Some("http://localhost:8000/v1")).is_ok());

    // A catalog the user extended with a custom provider accepts that id too.
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(
        dir.path().join("models-catalog.json"),
        r#"[{"id":"mybox","wire":"openai","model":"qwen3:8b","base_url":"http://127.0.0.1:9/v1"}]"#,
    )
    .expect("写目录文件");
    let extended = joyczl_provider::catalog::Catalog::load(dir.path());
    assert!(validate_provider(&extended, "mybox", None).is_ok());
}

/// The "which model is current" one-liner: with a model it names both, without it only the vendor.
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

// ---- End to end: really save, really switch, really remove (temp home, never touching the system keychain) ------------

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
    // Pin to the file backend: unit tests never touch the system keychain (macOS security would pop a
    // dialog and CI would hang). The temp dir is where this run's "fallback file" lives.
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

/// Save two profiles → switch → remove one: active follows all the way, and the file holds no key.
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

    // Only id + make_active = switch, no need to re-send the key.
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
    // The switch did not wash the config away: provider/model are still the original ones.
    let back = server.model_profiles();
    let ds = back
        .profiles
        .iter()
        .find(|p| p.id == "ds")
        .expect("ds 还在");
    assert_eq!(ds.provider, "deepseek");
    assert_eq!(ds.model.as_deref(), Some("deepseek-chat"));

    // Removing the current profile: active is cleared along with it, no dangling pointer.
    let removed = server.model_remove(ModelRemoveParams {
        id: "ds".to_string(),
    });
    assert!(removed.removed);
    assert!(removed.active_id.is_none());

    // The persisted file holds only config and pointers, never the key itself.
    let raw = std::fs::read_to_string(dir.path().join("models.json")).expect("models.json");
    assert!(!raw.contains("sk-"), "文件里不该有密钥：{raw}");
    assert!(raw.contains("env:JOY_TEST_KEY"), "凭据指针该在：{raw}");
}

/// An unknown provider is refused outright, with the list of available ones — not a bare "invalid params".
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

/// The first-time config also fails without a provider (it may be omitted only when switching).
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
