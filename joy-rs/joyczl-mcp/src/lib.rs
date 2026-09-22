//! MCP 连接器 —— 把任何 Model Context Protocol 服务器插成 Joy 自己的工具。
//!
//! loop 与 MCP 同在一个 async 运行时，没有跨运行时的桥接问题：工具
//! handler 返回的就是 future，所以这里就是「连上、
//! 列工具、调用时把 JSON 递过去」。
//!
//! 配置：`<home>/mcp.json`，两种传输按配置的形状挑（见 `config.rs`）：
//!
//! ```json
//! {"servers": [
//!   {"name": "fs", "command": "npx", "args": ["-y", "…/server-filesystem", "/tmp"]},
//!   {"name": "notes", "url": "https://host/mcp", "auth_env": "NOTES_API_KEY"}
//! ]}
//! ```
//!
//! 每个服务器的工具注册成 `<服务器>_<工具>`（名字会压到模型厂商肯收的形状，
//! 但回给服务器的永远是原名）。**连不上的服务器跳过并留一句警告，Joy 照样
//! 启动** —— 一个配错的服务器不该让整个助理起不来，跟工具执行失败不该让
//! 一轮对话崩掉是同一条规矩。
//!
//! 没有 `mcp.json` 就等于没配 MCP：不读文件、不起进程、不连网。

pub mod config;
pub mod oauth;
pub mod server;
pub mod transport;

use std::path::Path;
use std::sync::Arc;

use joyczl_tools::{Tool, ToolCtx};
use serde_json::Value;
use tokio::sync::Mutex;

use config::{Config, ServerSpec, Transport as TransportSpec};
use transport::{ToolMeta, Transport, TIMEOUT};

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod mcp_tests;

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod oauth_tests;

#[cfg(test)]
#[path = "mcp_server_tests.rs"]
mod mcp_server_tests;

/// 一条已经连上、握过手的连接。调用要排队，所以里面是一把 async 锁。
/// 熔断参数。默认抄 hermes：**连续 3 次失败开 60 秒断路器**。
#[derive(Debug, Clone, Copy)]
pub struct BreakerPolicy {
    pub failures: u32,
    pub cooldown: std::time::Duration,
}

impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            failures: 3,
            cooldown: std::time::Duration::from_secs(60),
        }
    }
}

/// 一台服务器的健康账。**按连接隔离**：一台挂了不该拖累另一台。
#[derive(Debug, Default)]
struct Breaker {
    consecutive_failures: u32,
    /// 断路器开到这个时刻（`None` = 关着）。
    open_until: Option<std::time::Instant>,
}

impl Breaker {
    /// 现在还能不能敲它的门。开着的话返回还要等多久。
    fn blocked_for(&mut self, now: std::time::Instant) -> Option<std::time::Duration> {
        match self.open_until {
            Some(until) if until > now => Some(until - now),
            // 冷却到了：**半开** —— 放一次探针过去，成不成看这一次。
            Some(_) => {
                self.open_until = None;
                None
            }
            None => None,
        }
    }

    fn record(&mut self, ok: bool, policy: &BreakerPolicy, now: std::time::Instant) {
        if ok {
            // 一次成功就清零：这是我们唯一能拿到的「它缓过来了」的证据。
            self.consecutive_failures = 0;
            self.open_until = None;
            return;
        }
        self.consecutive_failures += 1;
        if self.consecutive_failures >= policy.failures {
            self.open_until = Some(now + policy.cooldown);
        }
    }
}

pub struct Connection {
    name: String,
    transport: Mutex<Transport>,
    breaker: Mutex<Breaker>,
    policy: BreakerPolicy,
}

impl Connection {
    /// 造一条连接。`policy` 可注入，是为了让熔断的测试不必真等 60 秒。
    pub fn with_policy(name: &str, transport: Transport, policy: BreakerPolicy) -> Self {
        Self {
            name: name.to_string(),
            transport: Mutex::new(transport),
            breaker: Mutex::new(Breaker::default()),
            policy,
        }
    }

    /// 调一次工具。**永不返回 Err** —— 失败的文本是给模型读的，
    /// 跟 `ToolRegistry::execute` 同一条规矩。
    pub async fn call(&self, tool: &str, args: Value) -> String {
        // 熔断：连续失败到阈值就先别敲门了 —— 一台挂掉的服务器会让每一轮
        // 都白等一个超时，而模型还在那儿一遍遍地试。
        let now = std::time::Instant::now();
        if let Some(remaining) = self.breaker.lock().await.blocked_for(now) {
            return format!(
                "MCP {}_{tool} 被跳过了：这台服务器连续失败过 {} 次，断路器还开着（还有 {} 秒）—— \
                 等它凉下来会自动放一次探针。",
                self.name,
                self.policy.failures,
                remaining.as_secs().max(1)
            );
        }

        let mut transport = self.transport.lock().await;
        let outcome = tokio::time::timeout(TIMEOUT, transport.call_tool(tool, args)).await;
        drop(transport);

        let (ok, text) = match outcome {
            Ok(Ok(text)) => (true, text),
            Ok(Err(error)) => (
                false,
                format!("MCP 调用 {}_{tool} 失败：{error}", self.name),
            ),
            Err(_) => (
                false,
                format!(
                    "MCP 调用 {}_{tool} 失败：超过 {} 秒没有回话",
                    self.name,
                    TIMEOUT.as_secs()
                ),
            ),
        };
        let mut breaker = self.breaker.lock().await;
        breaker.record(ok, &self.policy, std::time::Instant::now());
        let opened = breaker.open_until.is_some() && !ok;
        drop(breaker);
        if opened {
            eprintln!(
                "(joy) MCP 服务器 '{}' 连续失败 {} 次，断路器打开 {} 秒",
                self.name,
                self.policy.failures,
                self.policy.cooldown.as_secs()
            );
        }
        text
    }
}

/// 一个连上的服务器：它的连接 + 它报上来的工具。
struct Server {
    name: String,
    connection: Arc<Connection>,
    tools: Vec<ToolMeta>,
}

/// MCP 的入口。`connect` 永不失败（失败变成警告），所以 app-server 可以
/// 无条件地在启动时调一次，配没配都一样。
#[derive(Default)]
pub struct McpClient {
    servers: Vec<Server>,
    warnings: Vec<String>,
}

impl McpClient {
    /// 读 `config_path` 并把里面每个服务器接上。没有这个文件就什么也不做。
    pub async fn connect(config_path: &Path) -> McpClient {
        let mut client = McpClient::default();
        if !config_path.exists() {
            return client; // 没配就是没配，不是错误
        }
        let config = match Config::read(config_path) {
            Ok(config) => config,
            Err(error) => {
                // 配置文件写坏了要看得见 —— 静默地「一个工具都没有」会让人
                // 以为服务器挂了。
                client.warnings.push(error);
                return client;
            }
        };
        for spec in &config.servers {
            client
                .connect_one(config_path.parent().unwrap_or(Path::new(".")), spec)
                .await;
        }
        client
    }

    async fn connect_one(&mut self, auth_root: &Path, spec: &ServerSpec) {
        if spec.name.is_empty() {
            self.warnings
                .push("有一条服务器没写 'name'，跳过".to_string());
            return;
        }
        let name = spec.name.clone();
        // 配置本身的毛病在读配置时就报掉，报的话术是「你哪里写错了」，
        // 而不是后面那种「连不上」。
        let lookup = |key: &str| std::env::var(key).ok();
        let mut target = match config::resolve(spec, &lookup) {
            Ok(target) => target,
            Err(message) => {
                self.warnings
                    .push(format!("MCP 服务器 '{name}' 跳过：{message}"));
                return;
            }
        };

        // oauth 服务器：bearer 从 mcp-auth 的 token 库里来，不在配置里。
        // 没登录过就警告 + 指路，Joy 照样启动。
        if spec.oauth.unwrap_or(false) {
            let url = spec.url.clone().unwrap_or_default();
            match oauth::ensure_access_token(auth_root, &name, &url).await {
                Ok(token) => {
                    if let TransportSpec::Http { token: slot, .. } = &mut target {
                        *slot = Some(token);
                    }
                }
                Err(error) => {
                    self.warnings
                        .push(format!("MCP 服务器 '{name}' 需要登录：{error}"));
                    return;
                }
            }
        }

        match self.open(&name, &target).await {
            Ok((connection, tools)) => {
                self.servers.push(Server {
                    name,
                    connection,
                    tools,
                });
            }
            Err(error) => {
                self.warnings
                    .push(format!("MCP 服务器 '{name}' 连不上：{error}"));
                if let Some(hint) = config::auth_hint(&target) {
                    self.warnings.push(hint);
                }
            }
        }
    }

    /// 打开传输、握手、列工具。三步都带超时：卡住的服务器不该拖住启动。
    async fn open(
        &self,
        name: &str,
        target: &TransportSpec,
    ) -> Result<(Arc<Connection>, Vec<ToolMeta>), String> {
        let mut transport = with_timeout("连接超时", Transport::connect(target)).await?;
        with_timeout("握手超时", transport.initialize()).await?;
        let tools = with_timeout("列工具超时", transport.list_tools()).await?;
        if tools.is_empty() {
            return Err("一个工具都没报上来".to_string());
        }
        Ok((
            Arc::new(Connection::with_policy(
                name,
                transport,
                BreakerPolicy::default(),
            )),
            tools,
        ))
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn servers(&self) -> Vec<&str> {
        self.servers.iter().map(|s| s.name.as_str()).collect()
    }

    /// 注册用的工具，按配置顺序。
    pub fn tools(&self) -> Vec<Tool> {
        let mut out = Vec::new();
        // 已经发出去的名字：两台服务器（或名字只差一个标点的两台）可能塌成同一个
        // 名字 —— 重名的加序号，谁也不覆盖谁。
        let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
        for server in &self.servers {
            for meta in &server.tools {
                let connection = server.connection.clone();
                // `tname` 是服务器**自己**的名字，没被改过：上面那个改名
                // 只是模型怎么称呼它，绝不是回线上时写的那个名字。
                let tname = meta.name.clone();
                let (name, renamed) =
                    config::model_safe_name_unique(&server.name, &meta.name, &mut taken);
                if renamed {
                    eprintln!(
                        "(joy) MCP 工具名冲突：{} 的 '{}' 改叫 '{}'（原名是模型可见的名字，改了要说出来）",
                        server.name, meta.name, name
                    );
                }
                out.push(Tool {
                    name,
                    description: format!(
                        "[MCP:{}] {}",
                        server.name,
                        meta.description.clone().unwrap_or_default()
                    ),
                    input_schema: meta.schema(),
                    handler: Arc::new(move |_ctx: ToolCtx, args: Value| {
                        let connection = connection.clone();
                        let tname = tname.clone();
                        Box::pin(async move { Ok(connection.call(&tname, args).await) })
                    }),
                });
            }
        }
        out
    }

    /// 直接调一次（测试与排障用；正常路径是注册成工具让模型调）。
    pub async fn call(&self, server: &str, tool: &str, args: Value) -> String {
        match self.servers.iter().find(|s| s.name == server) {
            Some(found) => found.connection.call(tool, args).await,
            None => format!("MCP 服务器 '{server}' 没有连上。"),
        }
    }
}

async fn with_timeout<T>(
    what: &str,
    future: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(TIMEOUT, future).await {
        Ok(result) => result,
        Err(_) => Err(format!("{what}（超过 {} 秒）", TIMEOUT.as_secs())),
    }
}
