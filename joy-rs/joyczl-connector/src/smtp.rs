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
        .map_err(|_| {
            anyhow::anyhow!(
                "the SMTP conversation made no progress for {}s; giving up",
                TIMEOUT.as_secs()
            )
        })?
}

async fn conversation(config: &SmtpConfig, message: &Outgoing) -> Result<()> {
    let tcp = TcpStream::connect((config.host.as_str(), config.port))
        .await
        .with_context(|| format!("could not connect to {}:{}", config.host, config.port))?;
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
            bail!("An SMTP user name is configured, but no password was found (the entry the secret points at does not exist)");
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
            bail!("the server does not accept recipient {to}: {code} {text}");
        }
    }
    session.command("DATA").await?;
    session.expect("354").await?;
    session
        .write_raw(render(config, message).as_bytes())
        .await
        .context("the connection dropped while sending the body")?;
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
            Wire::Tls(_) => bail!("internal error: the connection is already TLS when upgrading with STARTTLS — that is a connector bug"),
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
            bail!("the server does not support AUTH PLAIN ({code} {text}) — this connector only implements PLAIN")
        } else {
            bail!("SMTP authentication was rejected: {code} {text}")
        }
    }

    async fn command(&mut self, line: &str) -> Result<()> {
        self.write_raw(format!("{line}\r\n").as_bytes())
            .await
            .with_context(|| format!("the connection dropped while sending {line}"))
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
                .context("the connection dropped while waiting for the server")?;
            if read == 0 {
                bail!(
                    "the server disconnected mid-reply (received so far: {})",
                    lines.join(" | ")
                );
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
            bail!("expected {want}, the server sent {code} ({text})")
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
        .map_err(|_| anyhow::anyhow!("{host} is not a valid TLS host name"))?;
    connector
        .connect(server_name, tcp)
        .await
        .with_context(|| format!("the TLS handshake with {host} failed"))
}
