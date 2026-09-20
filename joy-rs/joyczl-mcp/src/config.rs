//! `mcp.json` 的读法，以及「这份配置到底想说什么」的判定。
//!
//! 配置里有两类事会当场被拒，理由是同一条：**同时写了两种说法，就等于没说**。
//! 哪条优先是猜的，而猜错的表现是「连上了但用的是另一种传输」这种最难查的
//! 故障，所以宁可拒掉。
//!
//! ```json
//! {"servers": [
//!   {"name": "fs", "command": "npx",
//!    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]},
//!   {"name": "notes", "url": "https://host/mcp", "auth_env": "NOTES_API_KEY"}
//! ]}
//! ```
//!
//! `auth_env` 里放的是**环境变量的名字**，不是凭证本身 —— mcp.json 是会被
//! 人粘进 bug report 的文件，里面躺一个 bearer token 就等于泄露。变量的值
//! 会作为 `Authorization: Bearer <值>` 发出去。

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// 一个服务器条目。字段都可选 —— 「必填」这件事由 [`resolve`] 按形状判断，
/// 这样报出来的错是「你两种都写了」，而不是 serde 的「缺字段」。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerSpec {
    pub name: String,
    pub url: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    pub auth_env: Option<String>,
    pub oauth: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub servers: Vec<ServerSpec>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        serde_json::from_str(text).map_err(|e| format!("mcp.json 不是合法 JSON：{e}"))
    }

    pub fn read(path: &Path) -> Result<Config, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("读不了 {}：{e}", path.display()))?;
        Config::parse(&text)
    }
}

/// 一个服务器条目解出来的连法。传输由配置的**形状**决定，而不是一个
/// `transport` 字段 —— 要么给本地命令，要么给远程 url。
#[derive(Debug, Clone, PartialEq)]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        /// `Some` 表示**只给这些变量**（设了 env 就是替换，
        /// 不是追加），`None` 表示继承当前进程的环境。
        env: Option<BTreeMap<String, String>>,
    },
    Http {
        url: String,
        /// 已经解析出来的 bearer 值（不是变量名）。
        token: Option<String>,
    },
}

/// 把一条配置解析成连法，或者说明它哪里不对。
///
/// `lookup_env` 传进来而不是直接读进程环境，是为了这条判定能被纯函数地测试。
pub fn resolve(
    spec: &ServerSpec,
    lookup_env: &dyn Fn(&str) -> Option<String>,
) -> Result<Transport, String> {
    let url = spec.url.as_deref().unwrap_or("");
    let command = spec.command.as_deref().unwrap_or("");

    if !url.is_empty() && !command.is_empty() {
        return Err("同时写了 'url' 和 'command' —— 只能挑一种传输".to_string());
    }
    if url.is_empty() && command.is_empty() {
        return Err("既没有 'url' 也没有 'command' —— 不知道该连什么".to_string());
    }

    if url.is_empty() {
        return Ok(Transport::Stdio {
            command: command.to_string(),
            args: spec.args.clone().unwrap_or_default(),
            env: spec.env.clone(),
        });
    }

    let auth_env = spec.auth_env.as_deref().unwrap_or("");
    let use_oauth = spec.oauth.unwrap_or(false);
    if !auth_env.is_empty() && use_oauth {
        return Err("同时写了 'auth_env' 和 'oauth' —— 只能挑一种".to_string());
    }
    // `oauth: true` 的服务器在这里长成「匿名 Http」：真正的 bearer 由上层
    //（lib.rs）从 mcp-auth 的 token 库里取 —— 没登录过的话，那边的报错
    // 会告诉人去跑 `joy mcp login`。见 oauth.rs。
    if !auth_env.is_empty() {
        // 不匿名连上去：那样服务器回 401，工具就静静地不见了，读起来
        // 像是「服务器挂了」，而不是「你没导出那个变量」。
        let token = lookup_env(auth_env)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("{auth_env} 没有值（mcp.json 里的 'auth_env' 指的是它）"))?;
        return Ok(Transport::Http {
            url: url.to_string(),
            token: Some(token),
        });
    }
    Ok(Transport::Http {
        url: url.to_string(),
        token: None,
    })
}

/// 远端服务器连不上时，告诉人**能自己查**的那一件事。
///
/// 这条提示只对 `auth_env` 那条路有意义：oauth 服务器走的是
/// `mcp-auth/` 里的 token，报错时已经各自说明了。
pub fn auth_hint(transport: &Transport) -> Option<String> {
    match transport {
        Transport::Http { url, token: None } => Some(format!(
            "  {url} —— 如果它需要鉴权，检查 'auth_env' 指的那个变量里有没有有效的凭证"
        )),
        _ => None,
    }
}

/// `<server>_<tool>`，压到模型厂商肯收的形状。
///
/// 工具名到了 Anthropic / OpenAI 那里要过 `^[a-zA-Z0-9_-]{1,64}$`；MCP 本身
/// 没有这个限制，而且点号分隔是常见写法（不少服务器发的是 `memory.remember`
/// 这种），于是「一个完全合法的 MCP 服务器」会让请求在模型看到任何东西之前
/// 就被拒掉。
///
/// 只改模型读到的那个名字。回给服务器的仍是原名，所以改名不可能弄坏派发。
pub fn model_safe_name(server: &str, tool: &str) -> String {
    let safe: String = format!("{server}_{tool}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.len() <= 64 {
        return safe;
    }
    // 保尾巴：区分度在工具自己的名字里，服务器前缀是读者损失得起的那部分。
    safe[safe.len() - 64..].trim_start_matches('_').to_string()
}
