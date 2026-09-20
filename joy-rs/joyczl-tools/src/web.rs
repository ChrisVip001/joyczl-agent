//! search_web —— 第二个真正的工具，LOOP 的最佳演示。
//!
//! 「查一下还剩哪几场世界杯，然后帮我排进日历」会横跨工具跑一圈：
//! search_web（读网络）→ 推理 → create_event。白板上 LOOP 那个框转起来。
//!
//! 两个后端：
//!   默认   DuckDuckGo HTML —— 无 key、零配置，够 demo（常被反爬挡）
//!   更好   Tavily —— 设了 `TAVILY_API_KEY`（或 `JOY_SEARCH_API_KEY`）就用它，
//!          agent 友好的搜索 API，结果干净（有免费额度）
//!
//! 返回给模型的是纯文本 —— Joy 从不替模型解析 HTML，只把可读的结果摆出来。

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

use crate::{opt_u32, require_str, Tool, ToolCtx};

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36";
const TIMEOUT: Duration = Duration::from_secs(20);

/// 一条搜索结果：标题、摘要、链接。
type Hit = (String, String, String);

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(TIMEOUT)
        .build()
        .map_err(Into::into)
}

async fn tavily(query: &str, key: &str, max_results: u32) -> Result<Vec<Hit>> {
    let body = json!({
        "api_key": key,
        "query": query,
        "max_results": max_results,
        "include_answer": false,
    });
    let data: Value = client()?
        .post("https://api.tavily.com/search")
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let hits = data
        .get("results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .map(|r| {
                    (
                        r.get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        truncate(r.get("content").and_then(Value::as_str).unwrap_or(""), 400),
                        r.get("url")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(hits)
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(max).collect();
        format!("{head}…")
    }
}

// ---- DuckDuckGo HTML 的抠法 -------------------------------------------------
//
// 不引 regex：页面里要找的就两种模式，逐段扫描反而比正则直白。
//   result__a" ... href="链接" ...>标题</a>
//   result__snippet" ...>摘要</a>

/// HTML 转义还原 + 去标签。Tavily 的结果不含 HTML；DDG 的两处都要过这里。
fn strip_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    let mut in_tag = false;
    while let Some(c) = chars.next() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            '&' if !in_tag => {
                // 实体到分号为止；认不出的原样留下。
                let mut entity = String::new();
                for e in chars.by_ref() {
                    if e == ';' {
                        break;
                    }
                    entity.push(e);
                    if entity.len() > 10 {
                        break;
                    }
                }
                out.push_str(&unescape_entity(&entity));
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

fn unescape_entity(entity: &str) -> String {
    match entity {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" | "#39" | "#x27" => "'".into(),
        "nbsp" => " ".into(),
        "hellip" | "#8230" | "#x2026" => "…".into(),
        _ => {
            // &#NN; 十进制数字码。
            if let Some(num) = entity.strip_prefix('#') {
                if let Ok(code) = num.parse::<u32>() {
                    if let Some(c) = char::from_u32(code) {
                        return c.to_string();
                    }
                }
            }
            format!("&{entity};")
        }
    }
}

/// 找 `marker` 之后第一个 `attr="value"` 里引号包住的值。
fn find_attr_after<'a>(page: &'a str, from: usize, attr: &str) -> Option<(usize, &'a str)> {
    let rest = &page[from..];
    let pattern = format!("{attr}=\"");
    let at = rest.find(&pattern)? + attr.len() + 2;
    let end = rest[at..].find('"')? + at;
    Some((end, &rest[at..end]))
}

/// 在 `from` 之后找 `<a ...>inner</a>` 的 inner 文本。
fn find_link_text(page: &str, from: usize) -> Option<(usize, String)> {
    let rest = &page[from..];
    let open = rest.find('>')? + 1;
    let close = rest[open..].find("</a>")? + open;
    Some((close + 4, strip_html(&rest[open..close])))
}

async fn duckduckgo(query: &str, max_results: u32) -> Result<Vec<Hit>> {
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencode(query));
    let page = client()?
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    // 每条结果都长成 `<a class="result__a" ... href="…">标题</a>`，
    // 摘要在它后面的 `<a class="result__snippet" ...>摘要</a>` 里。
    // 按 result__a 标记逐条推进 —— 不用正则，也不去猜页面结构。
    let mut hits: Vec<Hit> = Vec::new();
    let mut cursor = 0usize;
    while hits.len() < max_results as usize {
        let Some(marker_rel) = page[cursor..].find("result__a\"") else {
            break;
        };
        let marker = cursor + marker_rel;
        let Some((_after_href, href)) = find_attr_after(&page, marker, "href") else {
            break;
        };
        let Some((after_text, title)) = find_link_text(&page, marker) else {
            break;
        };
        // 摘要跟在标题后面；找不到就留空，不硬凑。
        let snippet = page[after_text..]
            .find("result__snippet\"")
            .and_then(|rel| find_link_text(&page, after_text + rel))
            .map(|(_, text)| text)
            .unwrap_or_default();
        // DDG 的结果包在一层重定向里，真正的目标在 uddg= 参数里。
        let target = href
            .find("uddg=")
            .map(|at| {
                let start = at + 5;
                let end = href[start..].find('&').unwrap_or(href.len() - start);
                urldecode(&href[start..start + end])
            })
            .unwrap_or_else(|| href.to_string());
        hits.push((title, snippet, target));
        cursor = after_text;
    }
    Ok(hits)
}

/// URL 编码（RFC 3986 未保留字符之外全部转义）。公开给同仓库的
/// joyczl-mcp 用 —— OAuth 的 authorize URL 和搜索跳转用的是同一条规则，
/// 两处各手抄一份只会慢慢漂移。
pub fn urlencode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// URL 解码：`%XX` 与 `+`（空格）。与 [`urlencode`] 成对。
pub fn urldecode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 搜索后端的 key：Tavily 的 key 有两个惯用名字，都认。
fn tavily_key() -> Option<String> {
    for name in ["TAVILY_API_KEY", "JOY_SEARCH_API_KEY"] {
        if let Ok(key) = std::env::var(name) {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return Some(key);
            }
        }
    }
    None
}

pub fn search_web() -> Tool {
    Tool {
        name: "search_web".to_string(),
        description: "搜索公共网页，拿回前几条结果（标题、摘要、URL）。\
                      用户问到时事、事实、日程、或任何你不确定的东西时用；\
                      拿到结果后再行动（比如把查到的日程 create_event 进日历）。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "搜索词"},
                "max_results": {"type": "integer", "description": "最多几条，默认 5"}
            },
            "required": ["query"]
        }),
        handler: Arc::new(|_ctx: ToolCtx, args: Value| {
            Box::pin(async move {
                let query = require_str(&args, "query")?;
                let max_results = opt_u32(&args, "max_results", 5)?.clamp(1, 10);

                let key = tavily_key();
                let outcome = match &key {
                    Some(key) => match tavily(&query, key, max_results).await {
                        Ok(hits) => Ok(("Tavily".to_string(), hits)),
                        Err(e) => {
                            // 有 key 的路径失败就直说 —— DDG 兜底反而会用
                            // 一个已知的坏后端再赌一次。
                            return Ok(format!(
                                "Web search failed ({e}). Answer from what you know, or ask the user."
                            ));
                        }
                    },
                    None => duckduckgo(&query, max_results)
                        .await
                        .map(|hits| ("DuckDuckGo".to_string(), hits)),
                };

                let (engine, hits) = outcome.unwrap_or_default();
                if hits.is_empty() {
                    return Ok(if key.is_none() {
                        "没有搜到结果 —— DuckDuckGo 的免费端点经常拦自动化请求。\
                         要稳定搜索，去 https://tavily.com 领一个免费 key 填进 .env \
                         （TAVILY_API_KEY）。现在先告诉用户搜不了，请他配 key。"
                            .to_string()
                    } else {
                        "No results found. Try a more specific query.".to_string()
                    });
                }
                let lines: Vec<String> = hits
                    .iter()
                    .enumerate()
                    .map(|(i, (title, snippet, link))| {
                        format!("{}. {title}\n   {snippet}\n   {link}", i + 1)
                    })
                    .collect();
                Ok(format!(
                    "Web results for '{query}' (via {engine}):\n{}",
                    lines.join("\n")
                ))
            })
        }),
    }
}
