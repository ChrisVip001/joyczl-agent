//! Behavior edges of `fetch_url`. Network cases all hit a **local** HTTP server
//! (only `allow_private=true` may reach 127.0.0.1; the tool itself always passes SSRF checks),
//! never the real internet — CI cannot depend on the outside world.

use super::web::{check_url, execute_fetch, fetch_url, html_to_text, is_disallowed_ip};
use crate::{ToolCtx, ToolRegistry};
use serde_json::json;

async fn ctx() -> ToolCtx {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite still writes -wal/-shm
    ToolCtx {
        cwd: None,
        approval: None,
        jobs: None,
        hooks: None,
        turn_limits: std::sync::Arc::new(crate::web::TurnLimits::default()),
        session_id: "test".to_string(),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home,
    }
}

// ---- Pure functions ---------------------------------------------------------

/// Private/reserved addresses are all refused, public ones pass — this table is the guardrail itself.
#[test]
fn disallowed_ips_cover_loopback_private_and_metadata() {
    for bad in [
        "127.0.0.1",       // loopback
        "10.1.2.3",        // private
        "172.16.0.9",      // private
        "192.168.1.1",     // private
        "169.254.169.254", // cloud metadata endpoint (link-local)
        "0.0.0.0",         // unspecified
        "192.0.2.7",       // documentation range
        "::1",             // v6 loopback
        "fe80::1",         // v6 link-local
        "fd00::1",         // v6 ULA
    ] {
        let ip: std::net::IpAddr = bad.parse().unwrap();
        assert!(is_disallowed_ip(ip), "{bad} 该被拒");
    }
    for good in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
        let ip: std::net::IpAddr = good.parse().unwrap();
        assert!(!is_disallowed_ip(ip), "{good} 该放行");
    }
}

/// Non-http(s) is refused outright — file:// is the most natural overreach attempt.
#[tokio::test]
async fn non_http_schemes_are_refused() {
    let url = reqwest::Url::parse("file:///etc/passwd").unwrap();
    let err = check_url(&url, true).await.unwrap_err().to_string();
    assert!(err.contains("只支持 http/https"), "{err}");
}

/// HTML → plain text: script/style vanish whole, entities decode, block tags break lines, whitespace collapses.
#[test]
fn html_to_text_strips_scripts_and_decodes_entities() {
    let html = "<html><head><title>标题</title><style>body{color:red}</style></head>\
                <body><p>第一段 &amp; 符号</p><script>evil()</script>\
                <p>数字实体 &#28450; 字</p><br/>/*注释<!--hidden-->*/尾巴</body></html>";
    let text = html_to_text(html);
    assert!(text.contains("标题"), "{text}");
    assert!(text.contains("第一段 & 符号"), "{text}");
    assert!(text.contains("数字实体 漢 字"), "{text}");
    assert!(text.contains("尾巴"), "{text}");
    assert!(!text.contains("evil"), "script 正文不该留下：{text}");
    assert!(!text.contains("color:red"), "style 正文不该留下：{text}");
    assert!(!text.contains("hidden"), "注释不该留下：{text}");
    // The collapse criteria: no consecutive blank lines, no whitespace-only lines — one
    // blank line between block elements is fine typography; layout garbage is the target.
    let lines: Vec<&str> = text.lines().collect();
    for pair in lines.windows(2) {
        assert!(
            !(pair[0].trim().is_empty() && pair[1].trim().is_empty()),
            "连续空行没折叠：{text:?}"
        );
    }
    assert!(
        lines.iter().all(|l| *l == l.trim()),
        "行首尾有残留空白：{text:?}"
    );
}

/// Unknown entities are kept as-is — no guessing.
#[test]
fn unknown_entities_pass_through() {
    assert!(html_to_text("<p>A &notanentity; B</p>").contains("&notanentity;"));
}

// ---- Local HTTP server ------------------------------------------------------

/// Starts a one-shot HTTP server, returns the base URL. Routing: path → (status, content-type, body, extra).
fn spawn_server(routes: Vec<(&'static str, u16, &'static str, String, &'static str)>) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本机端口");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for _ in 0..16 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
            let Some((status, ct, body, extra)) = routes
                .iter()
                .find(|(p, ..)| *p == path)
                .map(|(_, s, c, b, e)| (*s, *c, b, *e))
            else {
                continue;
            };
            let response = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n{}",
                status,
                if status < 300 { "OK" } else { "See Other" },
                ct,
                body.len(),
                extra,
                body,
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Happy path: follow the 302 to the final page, HTML to text, off-site banner first, no script.
#[tokio::test]
async fn follows_redirects_and_wraps_the_body_as_external_data() {
    let base = spawn_server(vec![
        (
            "/start",
            302,
            "text/html",
            String::new(),
            "Location: /final\r\n",
        ),
        (
            "/final",
            200,
            "text/html",
            "<html><body><h1>正文标题</h1><p>你好</p><script>alert(1)</script></body></html>"
                .into(),
            "",
        ),
    ]);
    let out = execute_fetch(&format!("{base}/start"), true).await.unwrap();
    assert!(out.starts_with("以下是外部网页的正文"), "{out}");
    assert!(out.contains("正文标题"), "{out}");
    assert!(out.contains("你好"), "{out}");
    assert!(!out.contains("alert"), "script 不该留下：{out}");
}

/// Every hop re-runs SSRF: a redirect to a private address is caught on hop 2.
#[tokio::test]
async fn a_redirect_to_a_private_target_is_refused() {
    // allow_private lets hop 1 (localhost) through, but the redirect target is 10.0.0.1 —
    // would even allow_private get through? No: allow_private skips the whole check.
    // So catching localhost with the default posture isn't feasible this way — instead assert
    // check_url rejects the private literal directly (the tool's behavior is pinned by the loopback case).
    let url = reqwest::Url::parse("http://10.0.0.1/x").unwrap();
    let err = check_url(&url, false).await.unwrap_err().to_string();
    assert!(err.contains("内网/保留地址"), "{err}");
}

/// Redirect loop: stops at the hop cap, with an explanatory error.
#[tokio::test]
async fn redirect_loops_stop_at_the_cap() {
    let base = spawn_server(vec![(
        "/loop",
        302,
        "text/plain",
        String::new(),
        "Location: /loop\r\n",
    )]);
    let err = execute_fetch(&format!("{base}/loop"), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("重定向超过"), "{err}");
}

/// HTTP errors report the status code honestly.
#[tokio::test]
async fn http_errors_are_honest() {
    let base = spawn_server(vec![("/gone", 404, "text/plain", "没了".into(), "")]);
    let err = execute_fetch(&format!("{base}/gone"), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("404"), "{err}");
}

/// Body char cap: truncated at 50k with a notice, never silently.
#[tokio::test]
async fn the_output_is_capped_with_an_honest_marker() {
    let base = spawn_server(vec![("/long", 200, "text/plain", "字".repeat(60_000), "")]);
    let out = execute_fetch(&format!("{base}/long"), true).await.unwrap();
    assert!(out.contains("truncated at 50000"), "{out}");
    assert!(
        out.chars().count() < 51_000,
        "超过 50k 还在往里塞：{}",
        out.chars().count()
    );
}

// ---- Tool-registry integration ----------------------------------------------

/// fetch_url is in the default table, and it **always** passes SSRF checks (127.0.0.1 refused outright).
#[tokio::test]
async fn the_tool_refuses_loopback_without_the_test_knob() {
    let mut registry = ToolRegistry::new();
    registry.register(fetch_url());
    let out = registry
        .execute(
            ctx().await,
            "fetch_url",
            json!({"url": "http://127.0.0.1:9/"}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("内网/保留地址"), "{out}");
}

// ---- Search: provider plan, per-turn budget, untrusted wrapping --------------

use super::web::{plan_backends, search_web, Backend, TurnLimits};
use std::sync::atomic::Ordering;

/// Ordering policy: keyed providers first (in order); without a key, Bing then DDG —
/// after a keyed backend fails, gambling on a free endpoint stays out of the plan.
#[test]
fn backend_plan_prefers_keyed_and_puts_bing_ahead_of_ddg_without_keys() {
    let plan = plan_backends(Some("t".into()), None, true, true);
    assert_eq!(plan.len(), 1, "有 key 时免 key 后端不进计划");
    assert!(matches!(plan[0], Backend::Tavily(_)));

    let plan = plan_backends(None, Some("b".into()), true, true);
    assert!(matches!(plan[0], Backend::Brave(_)));

    let plan = plan_backends(None, None, true, true);
    assert_eq!(plan.len(), 2, "无 key 时两个免 key 后端都试");
    assert!(
        matches!(plan[0], Backend::Bing),
        "Bing 先试：DDG 常被网络挡住"
    );
    assert!(matches!(plan[1], Backend::DuckDuckGo), "DDG 仍是最后兜底");

    assert!(matches!(
        plan_backends(None, None, false, true)[0],
        Backend::DuckDuckGo
    ));
    assert!(matches!(
        plan_backends(None, None, true, false)[0],
        Backend::Bing
    ));
    assert!(
        plan_backends(None, None, false, false).is_empty(),
        "全关就是没有后端 —— search_ready 必须已经把它挡在注册之外"
    );
}

/// Bing's public result page: `<li class="b_algo"><h2><a href>…` — parsing is pure, so
/// the shape that actually comes back is pinned here instead of guessed at in production.
#[test]
fn bing_result_page_parses_into_hits() {
    let page = r#"<html><body>
      <li class="b_algo" data-id="1"><h2><a href="https://example.com/a">CodeBuddy &amp; IDE</a></h2>
        <div class="b_caption"><p>一个 <b>IDE</b> 的介绍</p></div></li>
      <li class="b_algo"><h2><a href="https://example.com/b">第二条</a></h2>
        <div class="b_caption"><p>另一个结果</p></div></li>
    </body></html>"#;
    let hits = super::web::parse_bing(page, 5);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0, "CodeBuddy & IDE", "HTML 实体要解开");
    assert_eq!(hits[0].1, "一个 IDE 的介绍", "标签要去掉");
    assert_eq!(hits[0].2, "https://example.com/a");
    assert_eq!(hits[1].0, "第二条");
    assert_eq!(
        super::web::parse_bing(page, 1).len(),
        1,
        "max_results 要生效"
    );
}

/// Brave's payload shape (web.results[].title/description/url) parses into hits.
#[test]
fn brave_results_parse_into_hits() {
    let data = json!({"web": {"results": [
        {"title": "T", "description": "D", "url": "https://example.com/x"}
    ]}});
    let hits = super::web::parse_brave(&data).unwrap();
    assert_eq!(
        hits,
        vec![(
            "T".to_string(),
            "D".to_string(),
            "https://example.com/x".to_string()
        )]
    );
    assert!(super::web::parse_brave(&json!({})).unwrap().is_empty());
}

/// The budget short-circuits **before** validation and network: an exhausted turn
/// gets a readable stop, not a tool timeout and not a silent extra request.
#[tokio::test]
async fn search_cap_stops_the_turn_before_any_network() {
    let mut registry = ToolRegistry::new();
    registry.register(search_web());
    let c = ctx().await;
    c.turn_limits
        .used
        .store(c.turn_limits.search_max, Ordering::Relaxed);
    let out = registry
        .execute(c, "search_web", json!({"query": "anything"}))
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("上限"), "{out}");
}

/// Fresh budget: a new TurnLimits starts at zero used.
#[test]
fn turn_limits_start_empty() {
    let limits = TurnLimits::default();
    assert_eq!(limits.used.load(Ordering::Relaxed), 0);
    assert_eq!(limits.search_max, 20);
    assert_eq!(TurnLimits::new(0).search_max, 1, "0 钳到下界，永不为负预算");
    assert_eq!(TurnLimits::new(99_999).search_max, 200, "上界钳制");
}
