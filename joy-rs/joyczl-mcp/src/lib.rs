//! MCP connector — plug any Model Context Protocol server in as a Joy tool.
//!
//! loop and MCP live in the same async runtime; there is no cross-runtime bridge problem:
//! a tool handler returns a future, so this is just "connect, list tools, pass the JSON
//! through on call".
//!
//! Config: `<home>/mcp.json`; the transport is picked by the configured entry's shape (see `config.rs`):
//!
//! ```json
//! {"servers": [
//!   {"name": "fs", "command": "npx", "args": ["-y", "…/server-filesystem", "/tmp"]},
//!   {"name": "notes", "url": "https://host/mcp", "auth_env": "NOTES_API_KEY"}
//! ]}
//! ```
//!
//! Each server's tools are registered as `<server>_<tool>` (the name is squeezed into a
//! shape model vendors accept; the original name always goes back to the server).
//! **Unreachable servers are skipped with a warning and Joy still starts** — a misconfigured
//! server must not keep the whole assistant down, just as a tool failure must not crash a turn.
//!
//! Without `mcp.json` MCP is simply not configured: no file read, no process, no network.

pub mod config;
pub mod oauth;
pub mod server;
pub mod transport;

use std::path::{Path, PathBuf};
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

/// A connection that is up and handshaken. Calls queue, so inside is an async lock.
/// Breaker params: **3 consecutive failures open a 60-second circuit breaker**.
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

/// One server's health account. **Isolated per connection**: one down must not drag another.
#[derive(Debug, Default)]
struct Breaker {
    consecutive_failures: u32,
    /// The breaker is open until this instant (`None` = closed).
    open_until: Option<std::time::Instant>,
}

impl Breaker {
    /// Can we knock now? When open, returns how much longer to wait.
    fn blocked_for(&mut self, now: std::time::Instant) -> Option<std::time::Duration> {
        match self.open_until {
            Some(until) if until > now => Some(until - now),
            // Cooldown over: **half-open** — let one probe through; that probe decides.
            Some(_) => {
                self.open_until = None;
                None
            }
            None => None,
        }
    }

    fn record(&mut self, ok: bool, policy: &BreakerPolicy, now: std::time::Instant) {
        if ok {
            // One success resets the count: the only proof of recovery we can ever get.
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
    /// How to build a **new** transport when the old one dies. `None` = the connection was handed
    /// in already built (tests); there is nothing to rebuild from.
    spec: Option<TransportSpec>,
    breaker: Mutex<Breaker>,
    policy: BreakerPolicy,
}

/// Neither transport failure nor timeout proves that the server did not commit an action.
/// Reconnect prepares future calls; the failed call is never automatically replayed.
enum Failure {
    Transport(String),
    Timeout,
}

impl Connection {
    /// Build a connection. `policy` is injectable so breaker tests need not wait 60s.
    pub fn with_policy(name: &str, transport: Transport, policy: BreakerPolicy) -> Self {
        Self {
            name: name.to_string(),
            transport: Mutex::new(transport),
            spec: None,
            breaker: Mutex::new(Breaker::default()),
            policy,
        }
    }

    /// Remember the config this connection came from, so a dead transport can be rebuilt.
    pub fn with_spec(mut self, spec: TransportSpec) -> Self {
        self.spec = Some(spec);
        self
    }

    /// One try on the current transport.
    async fn attempt(&self, tool: &str, args: &Value) -> Result<String, Failure> {
        let mut transport = self.transport.lock().await;
        match tokio::time::timeout(TIMEOUT, transport.call_tool(tool, args.clone())).await {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(error)) => Err(Failure::Transport(error)),
            Err(_) => Err(Failure::Timeout),
        }
    }

    /// Rebuild from the spec and handshake again (`initialize` + `notifications/initialized`).
    async fn reconnect(&self) -> Result<(), String> {
        let Some(spec) = &self.spec else {
            return Err("this connection kept no configuration, so it cannot be rebuilt (restarting Joy reconnects)".to_string());
        };
        let mut fresh = Transport::connect(spec).await?;
        fresh.initialize().await?;
        *self.transport.lock().await = fresh;
        Ok(())
    }

    /// Call a tool. **Never returns Err** — the failure text is for the model to read,
    /// same rule as `ToolRegistry::execute`.
    pub async fn call(&self, tool: &str, args: Value) -> String {
        self.call_with_notice(tool, args, None).await
    }

    pub async fn call_with_notice(
        &self,
        tool: &str,
        args: Value,
        notice: Option<joyczl_tools::UncertainSink>,
    ) -> String {
        // Breaker: once failures hit the threshold, stop knocking — a dead server makes
        // every turn wait out a timeout while the model keeps trying again and again.
        let now = std::time::Instant::now();
        if let Some(remaining) = self.breaker.lock().await.blocked_for(now) {
            return format!(
                "Error: the MCP tool {}_{tool} was skipped: this server failed {} times in a row and \
                 the circuit breaker is still open ({} seconds left) — once it cools down one probe \
                 goes through by itself.",
                self.name,
                self.policy.failures,
                remaining.as_secs().max(1)
            );
        }

        let mut outcome = self.attempt(tool, &args).await;
        if let Err(Failure::Transport(first)) = &outcome {
            // A lost response may follow a committed side effect. Reconnect only for a
            // future explicitly requested call; do not repeat this unknown operation.
            let first = first.clone();
            match self.reconnect().await {
                Ok(()) => {
                    eprintln!(
                        "(joy) MCP server '{}' reconnected; {tool} outcome unknown, not replayed",
                        self.name
                    );
                    outcome = Err(Failure::Transport(format!("outcome unknown; connection restored but call was not replayed. Reconcile remote state before retrying: {first}")));
                }
                Err(why) => {
                    outcome = Err(Failure::Transport(format!("outcome unknown; reconcile remote state before retrying: {first}; the reconnect failed too: {why}")));
                }
            }
        }

        let uncertain = outcome.is_err();
        let (ok, text) = match outcome {
            Ok(text) => (true, text),
            Err(Failure::Transport(error)) => (
                false,
                format!("Error: the MCP call {}_{tool} failed: {error}", self.name),
            ),
            Err(Failure::Timeout) => (
                false,
                format!(
                    "Error: the MCP call {}_{tool} failed: no reply in over {} seconds; outcome unknown — reconcile remote state before retrying",
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
                "(joy) MCP server '{}' failed {} times in a row; circuit breaker open for {} seconds",
                self.name,
                self.policy.failures,
                self.policy.cooldown.as_secs()
            );
        }
        if uncertain {
            if let Some(notice) = notice {
                notice(&format!(
                    "MCP {}_{tool} outcome unknown; reconcile external state before continuing",
                    self.name
                ));
            }
        }
        text
    }
}

/// A connected server: its connection + the tools it reported.
struct Server {
    name: String,
    connection: Arc<Connection>,
    tools: Vec<ToolMeta>,
}

/// MCP's entry point. `connect` never fails (failures become warnings), so the app-server
/// can call it once at startup unconditionally, configured or not.
#[derive(Default)]
pub struct McpClient {
    servers: Vec<Server>,
    warnings: Vec<String>,
}

impl McpClient {
    /// Read `config_path` and connect every server inside. No file, nothing to do.
    pub async fn connect(config_path: &Path) -> McpClient {
        let mut client = McpClient::default();
        if !config_path.exists() {
            return client; // not configured, not an error
        }
        let config = match Config::read(config_path) {
            Ok(config) => config,
            Err(error) => {
                // A broken config file must be visible — silently "no tools at all" makes
                // people think the server is down.
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

    /// Connect `<home>/mcp.json` **plus** every `JOY_MCP_CONFIG` file (see
    /// [`config::config_paths_for`]). The user's file is read first and **wins on a
    /// same-named server** — local beats shared, the same rule skills use; a second
    /// copy under the same name is skipped, not double-registered.
    ///
    /// One broken extra file must not take the others down: its error is collected as a
    /// warning and the rest still connect (same fail-open discipline as one dead server).
    pub async fn connect_home(home: &Path) -> McpClient {
        let extra = std::env::var_os("JOY_MCP_CONFIG");
        Self::connect_paths(&config::config_paths_for(home, extra.as_deref())).await
    }

    /// Connect every config file in `paths`, in order; a same-named server in an
    /// earlier file wins (first wins, not last — the user's file comes first).
    async fn connect_paths(paths: &[PathBuf]) -> McpClient {
        let mut client = McpClient::default();
        for path in paths {
            let mut one = McpClient::connect(path).await;
            client.warnings.append(&mut one.warnings);
            for server in one.servers {
                if client.servers.iter().any(|s| s.name == server.name) {
                    continue;
                }
                client.servers.push(server);
            }
        }
        client
    }

    async fn connect_one(&mut self, auth_root: &Path, spec: &ServerSpec) {
        if spec.name.is_empty() {
            self.warnings
                .push("one server has no 'name'; skipped".to_string());
            return;
        }
        let name = spec.name.clone();
        // Problems with the config itself are reported at read time, phrased as "what you
        // wrote wrong", not the later "cannot connect" kind.
        let lookup = |key: &str| std::env::var(key).ok();
        let mut target = match config::resolve(spec, &lookup) {
            Ok(target) => target,
            Err(message) => {
                self.warnings
                    .push(format!("MCP server '{name}' skipped: {message}"));
                return;
            }
        };

        // OAuth servers: the bearer comes from mcp-auth's token store, not the config.
        // Never logged in → warn + point the way; Joy still starts.
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
                        .push(format!("MCP server '{name}' needs a sign-in: {error}"));
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
                    .push(format!("MCP server '{name}' cannot be reached: {error}"));
                if let Some(hint) = config::auth_hint(&target) {
                    self.warnings.push(hint);
                }
            }
        }
    }

    /// Open transport, handshake, list tools — all with a timeout; a stuck server must not stall startup.
    async fn open(
        &self,
        name: &str,
        target: &TransportSpec,
    ) -> Result<(Arc<Connection>, Vec<ToolMeta>), String> {
        let mut transport = with_timeout("connect timed out", Transport::connect(target)).await?;
        with_timeout("handshake timed out", transport.initialize()).await?;
        let tools = with_timeout("listing tools timed out", transport.list_tools()).await?;
        if tools.is_empty() {
            return Err("the server reported no tools at all".to_string());
        }
        Ok((
            Arc::new(
                Connection::with_policy(name, transport, BreakerPolicy::default())
                    // Keep the config: a dead transport is rebuilt from it, not from a restart.
                    .with_spec(target.clone()),
            ),
            tools,
        ))
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn servers(&self) -> Vec<&str> {
        self.servers.iter().map(|s| s.name.as_str()).collect()
    }

    /// Tools for registration, in config order.
    pub fn tools(&self) -> Vec<Tool> {
        let mut out = Vec::new();
        // Names already handed out: two servers (or two differing by one punctuation mark)
        // can collapse into the same name — numbered suffixes; nobody overwrites anybody.
        let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
        for server in &self.servers {
            for meta in &server.tools {
                let connection = server.connection.clone();
                // `tname` is the server's **own** name, never rewritten: the renaming above
                // is only what the model calls it, never the name written back on the wire.
                let tname = meta.name.clone();
                let (name, renamed) =
                    config::model_safe_name_unique(&server.name, &meta.name, &mut taken);
                if renamed {
                    eprintln!(
                        "(joy) MCP tool name collision: '{}' from {} is now called '{}' (the original is a model-visible name, so the change is said out loud)",
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
                        Box::pin(async move {
                            Ok(connection
                                .call_with_notice(&tname, args, _ctx.on_uncertain)
                                .await)
                        })
                    }),
                });
            }
        }
        out
    }

    /// Direct call (tests and debugging; the normal path registers it as a tool for the model).
    pub async fn call(&self, server: &str, tool: &str, args: Value) -> String {
        match self.servers.iter().find(|s| s.name == server) {
            Some(found) => found.connection.call(tool, args).await,
            None => format!("MCP server '{server}' is not connected."),
        }
    }
}

async fn with_timeout<T>(
    what: &str,
    future: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(TIMEOUT, future).await {
        Ok(result) => result,
        Err(_) => Err(format!("{what} (over {} seconds)", TIMEOUT.as_secs())),
    }
}
