//! hub 的测试：用户表、常量时间验证、名字的路径安全。
//!
//! 懒加载 `Hub::client` 会起真子进程 —— 那条路由冒烟覆盖（与单用户模式的
//! spawn 是同一行代码），这里只测纯逻辑。

use super::hub::{self, valid_user, Hub, Users};

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("临时目录")
}

/// 存进去的读得回来；文件是 0600（unix）。
#[test]
fn the_user_table_round_trips() {
    let dir = temp();
    let path = dir.path().join("users.json");
    let mut users = Users::load(&path).expect("缺文件 = 空表");
    assert!(users.is_empty());

    users.add("alice", "t-a");
    users.add("bob", "t-b");
    users.save(&path).expect("存盘");

    let loaded = Users::load(&path).expect("读回");
    assert_eq!(loaded.verify("t-a").as_deref(), Some("alice"));
    assert_eq!(loaded.verify("t-b").as_deref(), Some("bob"));
    assert_eq!(loaded.verify("t-c"), None, "没人对上就是没有");
    assert_eq!(loaded.len(), 2);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("元数据")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "token 文件必须只有主人能读");
    }
}

/// 同名 add 是覆盖，不是追加 —— 换 token 靠它。
#[test]
fn adding_the_same_user_replaces_the_token() {
    let mut users = Users::load(temp().path().join("users.json").as_path()).expect("空表");
    users.add("alice", "旧");
    users.add("alice", "新");
    assert_eq!(users.len(), 1);
    assert_eq!(users.verify("旧"), None);
    assert_eq!(users.verify("新").as_deref(), Some("alice"));
}

/// verify 是常量时间比较的语义：哪怕只差一个字节也对不上。
/// trim 是 resolve（Bearer 头解析）的事，verify 本身是精确匹配。
#[test]
fn verification_agrees_with_equality() {
    let mut users = Users::load(temp().path().join("users.json").as_path()).expect("空表");
    users.add("alice", "secret-token");
    assert_eq!(users.verify("secret-tokeN"), None, "差一个字符也不行");
    assert_eq!(
        users.verify("secret-token "),
        None,
        "verify 不 trim —— 那是 resolve 的事"
    );
}

/// 坏名字拒载 —— 装一个 `../` 等于给路径穿越开门。
#[test]
fn a_bad_name_refuses_to_load() {
    let dir = temp();
    let path = dir.path().join("users.json");
    std::fs::write(&path, r#"{"../evil": "t"}"#).expect("造坏条目");
    let error = Users::load(&path).unwrap_err();
    assert!(error.to_string().contains("不合格"), "{error}");
}

/// 名字的规矩：能进来的都是安全路径段。
#[test]
fn user_names_must_be_safe_path_segments() {
    assert!(valid_user("alice"));
    assert!(valid_user("a-1"));
    assert!(!valid_user("../evil"));
    assert!(!valid_user("/abs"));
    assert!(!valid_user(""));
    assert!(
        !valid_user("Alice"),
        "只许小写 —— 大小写不敏感的文件系统会撞名"
    );
    assert!(!valid_user("a".repeat(33).as_str()));
}

/// hub 打开时空 users.json 是配置错误，不是「等第一个用户」。
#[test]
fn an_empty_hub_is_a_configuration_error() {
    let dir = temp();
    let error = match Hub::open(dir.path()) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("空 hub 不该开得起来"),
    };
    assert!(error.contains("adduser"), "{error}");

    std::fs::write(dir.path().join("users.json"), r#"{"alice": "t-a"}"#).expect("造表");
    let hub = Hub::open(dir.path()).expect("有一个用户就能开");
    assert_eq!(hub.user_count(), 1);
    assert_eq!(hub.home("alice"), dir.path().join("alice"));
}

/// resolve：头形状不对/空 token → None；对上 → 用户。
#[test]
fn resolve_walks_the_bearer_header() {
    let dir = temp();
    std::fs::write(dir.path().join("users.json"), r#"{"alice": "t-a"}"#).expect("造表");
    let hub = Hub::open(dir.path()).expect("开 hub");

    assert_eq!(hub.resolve(None), None, "没带头");
    assert_eq!(hub.resolve(Some("t-a")), None, "裸 token 不算 Bearer");
    assert_eq!(hub.resolve(Some("Bearer ")), None, "空 token");
    assert_eq!(hub.resolve(Some("Bearer t-b")), None, "没人对上");
    assert_eq!(hub.resolve(Some("Bearer t-a")).as_deref(), Some("alice"));
}

/// user_count 只做统计；users 的锁不外泄（内部不变式，顺手钉住）。
#[test]
fn the_hub_reports_its_user_count() {
    let dir = temp();
    std::fs::write(
        dir.path().join("users.json"),
        r#"{"alice": "t-a", "bob": "t-b"}"#,
    )
    .expect("造表");
    let hub = Hub::open(dir.path()).expect("开 hub");
    assert_eq!(hub.user_count(), 2);
    // hub 模块本身也被引用，防止 unused 警告的另一种方式：断言模块路径存在。
    let _ = hub::valid_user("x");
}

// ---- M2c：BYOK 的隔离是结构性的 ------------------------------------------
//
// 密钥隔离不靠「记得别泄漏」，靠路径：每个用户的 home 是独立目录、独立的
// app-server 子进程、各自的 models.json 与钥匙串。这两条测试钉住结构本身。

/// 两个用户的 home 是根目录下两个不相交的目录 —— 这是「A 的 key 不出现在
/// B 的任何路径」的全部物理基础。
#[test]
fn two_users_never_share_a_home() {
    let dir = temp();
    std::fs::write(
        dir.path().join("users.json"),
        r#"{"alice": "t-a", "bob": "t-b"}"#,
    )
    .expect("造表");
    let hub = Hub::open(dir.path()).expect("开 hub");

    let alice = hub.home("alice");
    let bob = hub.home("bob");
    assert_ne!(alice, bob);
    assert!(alice.starts_with(dir.path()) && bob.starts_with(dir.path()));
    assert!(!alice.starts_with(&bob) && !bob.starts_with(&alice));
    // 名字规矩保证目录名就是用户名 —— 没有 `../` 能溜进路径。
    assert_eq!(alice.file_name().and_then(|n| n.to_str()), Some("alice"));
}

/// users.json 是准入账本，只存 token：它**永远**不该出现模型密钥的形状。
/// （密钥住在各用户 home 的钥匙串/secrets.json —— M1 的领地。）
#[test]
fn the_users_file_stores_tokens_never_model_keys() {
    let dir = temp();
    let path = dir.path().join("users.json");
    let mut users = Users::load(&path).expect("空表");
    users.add("alice", &super::hub::generate_token().expect("生成"));
    users.save(&path).expect("存盘");
    let raw = std::fs::read_to_string(&path).expect("读回");
    assert!(
        !raw.contains("sk-"),
        "准入账本里不该有模型密钥的形状：{raw}"
    );
    assert!(
        !raw.contains("keyEnv") && !raw.contains("apiKey"),
        "它只该有 token：{raw}"
    );
}
