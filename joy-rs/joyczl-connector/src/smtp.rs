//! A small SMTP sender — enough to deliver one message, nothing more.
//!
//! Why hand-written instead of an SMTP crate: what we need is a dial, EHLO, optionally
//! STARTTLS, optionally AUTH PLAIN, MAIL/RCPT/DATA, QUIT. That is ~200 lines on top of
//! tokio; a full client crate would bring its own connection pool, retry policy and
//! message model — none of which this feature wants.
//!
//! Ports decide the transport, as everywhere: 465 = implicit TLS, 587 = STARTTLS,
//! anything else = plaintext (local relay / the test server).
//!
//! One session = one `BufReader` for its whole life. Reading SMTP replies through a
//! fresh reader per command would drop whatever the server sent ahead — the kind of
//! bug that only shows up against a fast server.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader, ReadHalf, WriteHalf};
use tokio::net::TcpStream;

use crate::Outgoing;

/// Whole conversation cap: a stalled server must not hold a turn hostage.
const TIMEOUT: Duration = Duration::from_secs(45);

pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub from: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Send one message. Errors carry the step that failed and the server's own words —
/// "send failed" alone tells nobody whether to fix the host or the password.
pub async fn send(config: &SmtpConfig, message: &Outgoing) -> Result<()> {
    tokio::time::timeout(TIMEOUT, conversation(config, message))
        .await
        .map_err(|_| anyhow::anyhow!("SMTP 对话超过 {}s 没有进展，放弃了", TIMEOUT.as_secs()))?
}

async fn conversation(config: &SmtpConfig, message: &Outgoing) -> Result<()> {
    let tcp = TcpStream::connect((config.host.as_str(), config.port))
        .await
        .with_context(|| format!("连不上 {}:{}", config.host, config.port))?;
    let wire = if config.port == 465 {
        Wire::tls(tls_upgrade(tcp, &config.host).await?)
    } else {
        Wire::Plain(tcp)
    };
    let mut session = Session::new(wire);
    session.expect("220").await?;
    session.ehlo().await?;

    if config.port == 587 {
        session.command("STARTTLS").await?;
        session.expect("220").await?;
        // Hand the plain socket back, wrap it, and carry on in a fresh session.
        let plain = session.into_plain()?;
        let upgraded = tls_upgrade(plain, &config.host).await?;
        session = Session::new(Wire::tls(upgraded));
        session.ehlo().await?;
    }

    if let Some(user) = &config.username {
        let Some(password) = &config.password else {
            bail!("配了 SMTP 用户名，但没找到密码（secret 指向的条目不存在）");
        };
        session.auth_plain(user, password).await?;
    }

    session
        .command(&format!("MAIL FROM:<{}>", config.from))
        .await?;
    session.expect("250").await?;
    for to in &message.to {
        session.command(&format!("RCPT TO:<{to}>")).await?;
        // 250 or 251 (forwarded) both accept; anything else must stop the send —
        // delivering to half the recipients and claiming success would be a lie.
        let (code, text) = session.response().await?;
        if code != "250" && code != "251" {
            bail!("服务器不接受收件人 {to}：{code} {text}");
        }
    }
    session.command("DATA").await?;
    session.expect("354").await?;
    session
        .write_raw(render(config, message).as_bytes())
        .await
        .context("发正文时连接断了")?;
    session.expect("250").await?;

    // Polite shutdown; a server that hangs up first is not an error.
    let _ = session.command("QUIT").await;
    Ok(())
}

/// The wire format: MIME text/plain in UTF-8, base64 everywhere non-ASCII can appear.
///
/// base64 bodies are also why there is no dot-stuffing step: no base64 line can start
/// with `.`, so the end-of-data marker can only be ours.
fn render(config: &SmtpConfig, message: &Outgoing) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let subject = format!("=?UTF-8?B?{}?=", b64.encode(&message.subject));
    format!(
        "From: {}\r\n\
         To: {}\r\n\
         Subject: {}\r\n\
         Date: {}\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         {}\r\n\
         .\r\n",
        config.from,
        message.to.join(", "),
        subject,
        chrono::Utc::now().to_rfc2822(),
        wrap(&b64.encode(&message.body), 76),
    )
}

/// MIME wants lines ≤ 76 chars; one megabyte-long line is what some servers reject.
fn wrap(text: &str, width: usize) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / width + 1);
    for (index, chunk) in text.as_bytes().chunks(width).enumerate() {
        if index > 0 {
            out.push_str("\r\n");
        }
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
    }
    out
}

// ---- The stream: plain TCP or TLS, split into read/write halves -----------------------

enum Wire {
    Plain(TcpStream),
    // Boxed because the TLS stream is much larger than a socket; the enum is moved between
    // session halves on every STARTTLS upgrade, and clippy is right that copying it is waste.
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl Wire {
    fn tls(stream: tokio_rustls::client::TlsStream<TcpStream>) -> Self {
        Wire::Tls(Box::new(stream))
    }
}

impl AsyncRead for Wire {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Wire::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Wire::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Wire {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Wire::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Wire::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Wire::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Wire::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Wire::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Wire::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

struct Session {
    reader: BufReader<ReadHalf<Wire>>,
    writer: WriteHalf<Wire>,
}

impl Session {
    fn new(wire: Wire) -> Self {
        let (reader, writer) = tokio::io::split(wire);
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }

    /// Take the plaintext socket back out — only valid before TLS (STARTTLS's whole point).
    fn into_plain(self) -> Result<TcpStream> {
        let halves = self.reader.into_inner().unsplit(self.writer);
        match halves {
            Wire::Plain(tcp) => Ok(tcp),
            Wire::Tls(_) => bail!("内部错误：STARTTLS 升级时连接已经是 TLS —— 这是连接器的 bug"),
        }
    }

    async fn ehlo(&mut self) -> Result<()> {
        // The literal hostname in EHLO is informational; "localhost" is the safe neutral.
        self.command("EHLO localhost").await?;
        self.expect("250").await
    }

    async fn auth_plain(&mut self, user: &str, password: &str) -> Result<()> {
        use base64::Engine;
        let token =
            base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{password}"));
        self.command(&format!("AUTH PLAIN {token}")).await?;
        let (code, text) = self.response().await?;
        if code.starts_with('2') {
            Ok(())
        } else if text.contains("PLAIN") {
            bail!("服务器不支持 AUTH PLAIN（{code} {text}）—— 这个连接器只实现了 PLAIN")
        } else {
            bail!("SMTP 认证被拒：{code} {text}")
        }
    }

    async fn command(&mut self, line: &str) -> Result<()> {
        self.write_raw(format!("{line}\r\n").as_bytes())
            .await
            .with_context(|| format!("发命令 {line} 时连接断了"))
    }

    async fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        self.writer.write_all(bytes).await?;
        self.writer.flush().await.ok();
        Ok(())
    }

    /// One SMTP response: multiline replies read until `<code><space>`.
    async fn response(&mut self) -> Result<(String, String)> {
        let mut lines: Vec<String> = Vec::new();
        loop {
            let mut line = String::new();
            let read = self
                .reader
                .read_line(&mut line)
                .await
                .context("等服务器回话时连接断了")?;
            if read == 0 {
                bail!("服务器在回话中途断开了（已收到：{}）", lines.join(" | "));
            }
            let line = line.trim_end().to_string();
            let done = line.len() < 4 || line.as_bytes()[3] == b' ';
            lines.push(line);
            if done {
                break;
            }
        }
        let last = lines.last().cloned().unwrap_or_default();
        let code = last.get(..3).unwrap_or_default().to_string();
        Ok((code, lines.join(" | ")))
    }

    async fn expect(&mut self, want: &str) -> Result<()> {
        let (code, text) = self.response().await?;
        if code == want {
            Ok(())
        } else {
            bail!("期待 {want}，服务器回了 {code}（{text}）")
        }
    }
}

async fn tls_upgrade(
    tcp: TcpStream,
    host: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| anyhow::anyhow!("{host} 不是合法的 TLS 主机名"))?;
    connector
        .connect(server_name, tcp)
        .await
        .with_context(|| format!("和 {host} 的 TLS 握手失败"))
}
