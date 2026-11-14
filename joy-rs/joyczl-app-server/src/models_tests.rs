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
        vision: None,
        capabilities: None,
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
    assert!(out[0].active, "ds is the active profile");
    assert!(!out[1].active);
    assert_eq!(out[0].key_source.as_deref(), Some("keychain"));
    assert_eq!(out[1].key_source.as_deref(), Some("env:OPENAI_API_KEY"));
    let serialized = serde_json::to_string(&out).expect("serialize");
    assert!(
        !serialized.contains("sk-"),
        "the view must not carry keys: {serialized}"
    );
}

/// Provider validation: in-catalog ids pass; out-of-catalog ones need a base_url, otherwise the error lists what is available.
#[test]
fn provider_validation_accepts_known_ids_and_self_hosted_endpoints() {
    let catalog = joyczl_provider::catalog::Catalog::built_in();
    let known = catalog
        .ids()
        .first()
        .copied()
        .expect("at least one vendor in the catalog");
    assert!(validate_provider(&catalog, known, None).is_ok());

    let err = validate_provider(&catalog, "a vendor that does not exist", None).unwrap_err();
    assert!(err.contains("unknown provider"), "{err}");
    assert!(
        err.contains(known),
        "the error should list what is available: {err}"
    );

    // Self-hosted endpoint: the provider only decides which wire format is used.
    assert!(validate_provider(
        &catalog,
        "a vendor that does not exist",
        Some("http://localhost:8000/v1")
    )
    .is_ok());

    // A catalog the user extended with a custom provider accepts that id too.
    let dir = tempfile::tempdir().expect("temporary directory");
    std::fs::write(
        dir.path().join("models-catalog.json"),
        r#"[{"id":"mybox","wire":"openai","model":"qwen3:8b","base_url":"http://127.0.0.1:9/v1"}]"#,
    )
    .expect("write the catalog file");
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

    settings.model = Some("deepseek-flash".to_string());
    assert_eq!(
        current_label(&settings).as_deref(),
        Some("deepseek / deepseek-flash")
    );

    let empty = joyczl_config::Settings {
        provider: String::new(),
        ..joyczl_config::Settings::default()
    };
    assert_eq!(current_label(&empty), None);
}

// ---- End to end: really save, really switch, really remove (temp home, never touching the system keychain) ------------

async fn server() -> (Server, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
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
        .expect("the secrets lock should not be poisoned")
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
        vision: None,
        capabilities: None,
        make_active: None,
    }
}

/// Save two profiles → switch → remove one: active follows all the way, and the file holds no key.
#[tokio::test]
async fn saving_switching_and_removing_end_to_end() {
    let (server, dir) = server().await;

    let first = server
        .model_save(save("ds", "deepseek", Some("deepseek-flash")))
        .expect("store the first profile");
    assert!(first.active, "once stored it should be the active one");
    assert_eq!(first.key_stored_in, "env:JOY_TEST_KEY");
    assert_eq!(first.current.as_deref(), Some("deepseek / deepseek-flash"));

    server
        .model_save(save("oa", "openai", Some("gpt-5.2")))
        .expect("store the second profile");
    let list = server.model_profiles();
    assert_eq!(
        list.active_id.as_deref(),
        Some("oa"),
        "the one saved later became current"
    );
    assert_eq!(list.profiles.len(), 2);
    assert!(list.profiles.iter().all(|p| p.key_source.is_some()));

    // Only id + make_active = switch, no need to re-send the key.
    let switched = server
        .model_save(ModelSaveParams {
            provider: None,
            model: None,
            ..save("ds", "", None)
        })
        .expect("switch");
    assert!(switched.active);
    assert_eq!(
        server.model_profiles().active_id.as_deref(),
        Some("ds"),
        "switched back"
    );
    // The switch did not wash the config away: provider/model are still the original ones.
    let back = server.model_profiles();
    let ds = back
        .profiles
        .iter()
        .find(|p| p.id == "ds")
        .expect("ds is still there");
    assert_eq!(ds.provider, "deepseek");
    assert_eq!(ds.model.as_deref(), Some("deepseek-flash"));

    // Removing the current profile: active is cleared along with it, no dangling pointer.
    let removed = server.model_remove(ModelRemoveParams {
        id: "ds".to_string(),
    });
    assert!(removed.removed);
    assert!(removed.active_id.is_none());

    // The persisted file holds only config and pointers, never the key itself.
    let raw = std::fs::read_to_string(dir.path().join("models.json")).expect("models.json");
    assert!(!raw.contains("sk-"), "no key may appear in the file: {raw}");
    assert!(
        raw.contains("env:JOY_TEST_KEY"),
        "the credential pointer should be there: {raw}"
    );
}

/// An unknown provider is refused outright, with the list of available ones — not a bare "invalid params".
#[tokio::test]
async fn an_unknown_provider_is_refused_with_the_list() {
    let (server, _dir) = server().await;
    let err = server
        .model_save(save("x", "a vendor that does not exist", None))
        .unwrap_err();
    assert!(err.contains("unknown provider"), "{err}");
    assert!(
        err.contains(joyczl_provider::PROVIDERS[0].id),
        "the error should list what is available: {err}"
    );
    assert!(
        server.model_profiles().profiles.is_empty(),
        "nothing got stored"
    );
}

/// The first-time config also fails without a provider (it may be omitted only when switching).
#[tokio::test]
async fn a_new_profile_needs_a_provider() {
    let (server, _dir) = server().await;
    let err = server
        .model_save(ModelSaveParams {
            provider: None,
            ..save("bare", "", None)
        })
        .unwrap_err();
    assert!(err.contains("first configuration"), "{err}");
}

/// Declaring vision while adding the profile: said once, instead of sending an image, getting
/// refused and coming back to edit the catalog file.
#[tokio::test]
async fn a_profile_can_declare_vision_over_the_vendor_default() {
    let (server, _dir) = server().await;
    // deepseek's catalog entry is vision:false (its default model is text-only) — this profile says
    // it can see.
    server
        .model_save(ModelSaveParams {
            api_key: Some("sk-vision".to_string()),
            key_env: None,
            vision: Some(true),
            ..save("ds", "deepseek", Some("deepseek-v4.1-flash"))
        })
        .expect("store a profile that declares vision");

    assert!(
        server.resolved().expect("it should parse").vision,
        "the parsed result has to carry the declaration, or the door still blocks images"
    );
    assert_eq!(
        server.model_profiles().profiles[0].vision,
        Some(true),
        "the list has to show it honestly too"
    );

    // Changing only the model name, not mentioning vision: the declaration has to survive (patch
    // semantics, the same as model/base_url).
    server
        .model_save(ModelSaveParams {
            model: Some("deepseek-v4.1-flash".to_string()),
            key_env: None,
            ..save("ds", "deepseek", None)
        })
        .expect("change the model");
    assert_eq!(
        server.model_profiles().profiles[0].vision,
        Some(true),
        "not mentioning vision must not wipe it out"
    );
}

#[tokio::test]
async fn capabilities_are_validated_before_credentials_and_survive_profile_patches() {
    let (server, home) = server().await;
    let mut bad = save("oa", "openai", Some("gpt-6.1-sol"));
    bad.api_key = Some("synthetic-test-secret".into());
    bad.capabilities = Some(joyczl_protocol::ModelCapabilities {
        reasoning_effort: Some("none".into()),
        ..Default::default()
    });
    assert!(server.model_save(bad).is_err());
    assert!(!home.path().join("secrets.json").exists());
    assert!(server.model_profiles().profiles.is_empty());
    let mut params = save("oa", "openai", Some("gpt-6.1-sol"));
    params.capabilities = Some(joyczl_protocol::ModelCapabilities {
        tools: Some(false),
        context_window: Some(100_000),
        reasoning_effort: Some("high".into()),
    });
    server.model_save(params).unwrap();
    let expected = server.model_profiles().profiles[0].capabilities.clone();
    server
        .model_save(ModelSaveParams {
            provider: None,
            ..save("oa", "", None)
        })
        .unwrap();
    assert_eq!(server.model_profiles().profiles[0].capabilities, expected);
    assert_eq!(
        joyczl_config::models::load_models(home.path()).profiles[0].capabilities,
        expected
    );
    server
        .model_save(ModelSaveParams {
            capabilities: Some(Default::default()),
            ..save("oa", "openai", None)
        })
        .unwrap();
    assert_eq!(
        server.model_profiles().profiles[0].capabilities,
        Some(Default::default())
    );
}
