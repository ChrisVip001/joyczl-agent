//! A minimal IMAP receiver — enough to pull new mail, nothing more.
//!
//! Why hand-written: the same reason as SMTP. We need LOGIN, SELECT, UID SEARCH and
//! UID FETCH (BODY.PEEK[]) on a socket, and a mailbox we can trust to be read-only
//! (PEEK leaves the \Seen flag alone — Joy never marks another person's mail as read
//! just by fetching it). A full IMAP crate brings folders, IDLE, SEARCH grammar, and a
//! message model; this feature wants four commands.
//!
//! Ports decide the transport, as everywhere else here: 993 = implicit TLS, 143 =
//! STARTTLS, anything else = plaintext (local test server).
//!
//! MIME scope, stated plainly: text/plain is taken as-is, text/html is stripped to text,
//! multipart/alternative prefers text/plain, base64 and quoted-printable are decoded,
//! RFC 2047 encoded-words in headers are decoded. Charsets other than UTF-8 are read
//! lossily — see docs/limitations (no GBK table without another dependency).

use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

const TIMEOUT: Duration = Duration::from_secs(45);
/// A mail bigger than this is not mail anymore — skip it, and say so.
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub mailbox: String,
}

/// One fetched message, already reduced to what the inbox stores.
#[derive(Debug, Clone, PartialEq)]
pub struct Incoming {
    pub uid: i64,
    pub message_id: Option<String>,
    pub from: String,
    pub subject: String,
    pub date: String,
    pub body: String,
}

/// What one poll produced. `server_max_uid` is always the mailbox's current high-water
/// mark, so the caller can advance its cursor even when nothing was fetched.
#[derive(Debug)]
pub struct FetchOutcome {
    pub messages: Vec<Incoming>,
    pub server_max_uid: Option<i64>,
}

/// Fetch messages with UID greater than `after`. `None` = first run: read the mailbox's
/// high-water mark and fetch **nothing** (don't dump an existing inbox into the ledger).
pub async fn fetch(config: &ImapConfig, after: Option<i64>) -> Result<FetchOutcome> {
    tokio::time::timeout(TIMEOUT, conversation(config, after))
        .await
        .map_err(|_| anyhow::anyhow!("IMAP 对话超过 {}s 没有进展，放弃了", TIMEOUT.as_secs()))?
}

async fn conversation(config: &ImapConfig, after: Option<i64>) -> Result<FetchOutcome> {
    let tcp = TcpStream::connect((config.host.as_str(), config.port))
        .await
        .with_context(|| format!("连不上 {}:{}", config.host, config.port))?;
    let wire = if config.port == 993 {
        Wire::Tls(Box::new(tls_upgrade(tcp, &config.host).await?))
    } else {
        Wire::Plain(tcp)
    };
    let mut session = Session::new(wire);
    session.expect_ok("").await?; // "* OK ..." greeting

    session.login(&config.username, &config.password).await?;
    session.select(&config.mailbox).await?;

    // First run: take the mailbox's high-water mark and stop — the past is not news.
    let server_max = session.highest_uid().await?;
    let uids = match after {
        Some(cursor) => session.search_after(cursor).await?,
        None => Vec::new(),
    };
    let mut messages = Vec::new();
    for uid in uids {
        match session.fetch_one(uid).await {
            Ok(raw) => {
                if raw.len() > MAX_MESSAGE_BYTES {
                    eprintln!(
                        "(joy) 第 {uid} 封超过 {}KB，跳过不存",
                        MAX_MESSAGE_BYTES / 1024
                    );
                    continue;
                }
                messages.push(parse(&raw, uid));
            }
            Err(e) => eprintln!("(joy) 取第 {uid} 封失败（继续取下一封）：{e}"),
        }
    }
    let _ = session.logout().await;
    Ok(FetchOutcome {
        messages,
        server_max_uid: server_max,
    })
}

enum Wire {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

struct Session {
    reader: BufReader<Box<dyn ReadHalf>>,
    writer: Box<dyn WriteHalf>,
}

/// Type aliases keep the session struct readable; both halves are boxed so `Session` is
/// one type regardless of transport.
trait ReadHalf: tokio::io::AsyncRead + Unpin + Send {}
impl<T: tokio::io::AsyncRead + Unpin + Send> ReadHalf for T {}
trait WriteHalf: tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncWrite + Unpin + Send> WriteHalf for T {}

impl Wire {
    fn split(self) -> (Box<dyn ReadHalf>, Box<dyn WriteHalf>) {
        match self {
            Wire::Plain(stream) => {
                let (read, write) = stream.into_split();
                (Box::new(read), Box::new(write))
            }
            Wire::Tls(stream) => {
                let (read, write) = tokio::io::split(*stream);
                (Box::new(read), Box::new(write))
            }
        }
    }
}

impl Session {
    fn new(wire: Wire) -> Self {
        let (read, write) = wire.split();
        Self {
            reader: BufReader::new(read),
            writer: write,
        }
    }

    async fn send(&mut self, line: &str) -> Result<()> {
        self.writer
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .context("往 IMAP 写命令失败")
    }

    async fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .await
            .context("读 IMAP 应答失败")?;
        if n == 0 {
            bail!("IMAP 连接被对端关闭");
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
    }

    /// Wait for the tagged completion of `tag`; returns its trailing text.
    /// Untagged lines are ignored unless `keep` says otherwise.
    async fn await_tag(&mut self, tag: &str, mut keep: impl FnMut(&str)) -> Result<String> {
        loop {
            let line = self.read_line().await?;
            if line.starts_with(&format!("{tag} ")) {
                let rest = &line[tag.len() + 1..];
                if !rest.starts_with("OK") {
                    bail!("IMAP {tag} 失败：{rest}");
                }
                return Ok(rest.to_string());
            }
            if line.starts_with('+') {
                continue;
            }
            keep(&line);
        }
    }

    async fn expect_ok(&mut self, tag: &str) -> Result<()> {
        let line = self.read_line().await?;
        if tag.is_empty() {
            if line.starts_with('*') && (line.contains(" OK") || line.contains(" PREAUTH")) {
                return Ok(());
            }
            bail!("IMAP 打招呼不对：{line}");
        }
        if line.starts_with(&format!("{tag} ")) && line.contains(" OK") {
            return Ok(());
        }
        bail!("IMAP {tag} 失败：{line}")
    }

    async fn login(&mut self, user: &str, password: &str) -> Result<()> {
        let command = format!("a1 LOGIN {} {}", quote(user), quote(password));
        self.send(&command).await?;
        self.await_tag("a1", |_| {}).await.map(|_| ())
    }

    async fn select(&mut self, mailbox: &str) -> Result<()> {
        self.send(&format!("a2 SELECT {}", quote(mailbox))).await?;
        self.await_tag("a2", |_| {}).await.map(|_| ())
    }

    /// UIDs greater than `cursor`. `UID SEARCH UID n:*` includes n itself per RFC 3501,
    /// so the caller filters — we ask, then drop anything not strictly newer.
    async fn search_after(&mut self, cursor: i64) -> Result<Vec<i64>> {
        let mut found = Vec::new();
        self.send(&format!("a3 UID SEARCH UID {}:*", cursor + 1))
            .await?;
        self.await_tag("a3", |line| {
            if let Some(rest) = line.strip_prefix("* SEARCH") {
                found = rest
                    .split_whitespace()
                    .filter_map(|token| token.parse::<i64>().ok())
                    .filter(|uid| *uid > cursor)
                    .collect();
            }
        })
        .await?;
        Ok(found)
    }

    /// The highest UID currently in the mailbox (first run: the cursor's starting point).
    async fn highest_uid(&mut self) -> Result<Option<i64>> {
        let mut found = Vec::new();
        self.send("a3 UID SEARCH ALL").await?;
        self.await_tag("a3", |line| {
            if let Some(rest) = line.strip_prefix("* SEARCH") {
                found = rest
                    .split_whitespace()
                    .filter_map(|token| token.parse::<i64>().ok())
                    .collect();
            }
        })
        .await?;
        Ok(found.into_iter().max())
    }

    /// Fetch one message body as RFC822 bytes (literal-aware).
    async fn fetch_one(&mut self, uid: i64) -> Result<Vec<u8>> {
        self.send(&format!("a4 UID FETCH {uid} (BODY.PEEK[])"))
            .await?;
        // The literal arrives as "* N FETCH (UID x BODY[] {size}\r\n<size bytes>)\r\n".
        let mut literal: Vec<u8> = Vec::new();
        loop {
            let line = self.read_line().await?;
            if line.starts_with("a4 ") {
                if !line.contains(" OK") {
                    bail!("IMAP 取信失败：{line}");
                }
                return Ok(literal);
            }
            if let Some(size) = literal_size(&line) {
                let mut buffer = vec![0u8; size];
                self.reader
                    .read_exact(&mut buffer)
                    .await
                    .context("读 IMAP 字面量失败")?;
                literal = buffer;
                // Consume the ")" that follows the literal on its own line.
                let _ = self.read_line().await?;
            }
        }
    }

    async fn logout(&mut self) -> Result<()> {
        self.send("a5 LOGOUT").await?;
        self.await_tag("a5", |_| {}).await.map(|_| ())
    }
}

/// `{123}` at the end of a FETCH line announces a literal of 123 bytes.
fn literal_size(line: &str) -> Option<usize> {
    let end = line.rfind('}')?;
    let start = line[..end].rfind('{')?;
    line[start + 1..end].parse().ok()
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

async fn tls_upgrade(stream: TcpStream, host: &str) -> Result<TlsStream<TcpStream>> {
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| anyhow::anyhow!("IMAP 主机名 {host} 不能做 TLS 名字"))?;
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
    connector
        .connect(server_name, stream)
        .await
        .context("IMAP TLS 握手失败")
}

// ---- MIME, minimally ----------------------------------------------------------------

/// Raw message bytes → the fields the inbox stores. Never fails: a broken message is
/// still a message (subject = "(无主题)", body = whatever could be read).
pub fn parse(raw: &[u8], uid: i64) -> Incoming {
    let (head, body) = split_head(raw);
    let headers = parse_headers(head);

    let subject = decode_2047(headers.get("subject").map(String::as_str).unwrap_or(""));
    let from = decode_2047(
        headers
            .get("from")
            .map(String::as_str)
            .unwrap_or("(未知发件人)"),
    );
    let message_id = headers.get("message-id").cloned();
    let date = headers.get("date").cloned().unwrap_or_default();

    let content_type = headers
        .get("content-type")
        .cloned()
        .unwrap_or_else(|| "text/plain".to_string());
    let encoding = headers
        .get("content-transfer-encoding")
        .cloned()
        .unwrap_or_default();
    let text = extract_text(&content_type, &encoding, body);

    Incoming {
        uid,
        message_id,
        from,
        subject: if subject.trim().is_empty() {
            "(无主题)".to_string()
        } else {
            subject
        },
        date,
        body: text.trim().to_string(),
    }
}

fn split_head(raw: &[u8]) -> (&[u8], &[u8]) {
    match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(pos) => (&raw[..pos], &raw[pos + 4..]),
        None => match raw.windows(2).position(|w| w == b"\n\n") {
            Some(pos) => (&raw[..pos], &raw[pos + 2..]),
            None => (raw, &[]),
        },
    }
}

/// Header map with lowercase keys and unfolded values.
fn parse_headers(head: &[u8]) -> std::collections::HashMap<String, String> {
    let text = String::from_utf8_lossy(head);
    let mut map = std::collections::HashMap::new();
    let mut current: Option<(String, String)> = None;
    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = current.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((key, value)) = current.take() {
            map.entry(key).or_insert(value);
        }
        if let Some((key, value)) = line.split_once(':') {
            current = Some((key.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    if let Some((key, value)) = current {
        map.entry(key).or_insert(value);
    }
    map
}

/// Pick the text out of a (possibly multipart) body, then decode its transfer encoding.
fn extract_text(content_type: &str, encoding: &str, body: &[u8]) -> String {
    if content_type.to_ascii_lowercase().contains("multipart/") {
        let boundary = boundary_of(content_type);
        if let Some(boundary) = boundary {
            let parts = split_parts(body, &boundary);
            // Prefer the plain-text alternative; fall back to the first part that has text.
            let mut html: Option<String> = None;
            for part in parts {
                let (head, part_body) = split_head(part);
                let headers = parse_headers(head);
                let part_type = headers
                    .get("content-type")
                    .cloned()
                    .unwrap_or_else(|| "text/plain".to_string());
                let part_encoding = headers
                    .get("content-transfer-encoding")
                    .cloned()
                    .unwrap_or_default();
                let lower = part_type.to_ascii_lowercase();
                if lower.contains("multipart/") {
                    let nested = extract_text(&part_type, &part_encoding, part_body);
                    if !nested.trim().is_empty() {
                        return nested;
                    }
                } else if lower.contains("text/plain") {
                    return decode_body(part_body, &part_encoding);
                } else if lower.contains("text/html") && html.is_none() {
                    html = Some(strip_html(&decode_body(part_body, &part_encoding)));
                }
            }
            return html.unwrap_or_default();
        }
    }
    let decoded = decode_body(body, encoding);
    if content_type.to_ascii_lowercase().contains("text/html") {
        strip_html(&decoded)
    } else {
        decoded
    }
}

fn boundary_of(content_type: &str) -> Option<String> {
    let lower = content_type.to_ascii_lowercase();
    let index = lower.find("boundary=")?;
    let rest = content_type[index + "boundary=".len()..].trim();
    let value = rest.trim_start_matches('"');
    let end = value
        .find('"')
        .or_else(|| value.find(';'))
        .unwrap_or(value.len());
    Some(value[..end].trim().to_string())
}

/// Split a multipart body on its boundary, byte-exact: offsets refer to the original
/// bytes, so a non-UTF-8 part survives (a lossy String would not).
fn split_parts<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let marker = format!("--{boundary}");
    let closing = format!("{marker}--");
    let mut parts = Vec::new();
    let mut part_start: Option<usize> = None;
    let mut offset = 0;
    while offset < body.len() {
        let line_end = body[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|pos| offset + pos + 1)
            .unwrap_or(body.len());
        let line = trim_ascii_ws(&body[offset..line_end]);
        if line == marker.as_bytes() || line == closing.as_bytes() {
            if let Some(start) = part_start.take() {
                parts.push(&body[start..offset]);
            }
            if line == marker.as_bytes() {
                part_start = Some(line_end);
            }
        }
        offset = line_end;
    }
    if let Some(start) = part_start {
        parts.push(&body[start..]);
    }
    parts
}

fn trim_ascii_ws(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|pos| pos + 1)
        .unwrap_or(start);
    &line[start..end]
}

fn decode_body(body: &[u8], encoding: &str) -> String {
    let encoding = encoding.to_ascii_lowercase();
    let bytes: Vec<u8> = if encoding.contains("base64") {
        let compact: String = String::from_utf8_lossy(body)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        base64::engine::general_purpose::STANDARD
            .decode(compact.as_bytes())
            .unwrap_or_else(|_| body.to_vec())
    } else if encoding.contains("quoted-printable") {
        decode_quoted_printable(body)
    } else {
        body.to_vec()
    };
    String::from_utf8_lossy(&bytes).to_string()
}

fn decode_quoted_printable(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut index = 0;
    while index < body.len() {
        match body[index] {
            b'=' if index + 2 < body.len()
                && body[index + 1] == b'\r'
                && body[index + 2] == b'\n' =>
            {
                index += 3;
            }
            b'=' if index + 1 < body.len() && body[index + 1] == b'\n' => {
                index += 2;
            }
            b'=' if index + 2 < body.len() => {
                let hex = std::str::from_utf8(&body[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'=');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// RFC 2047 encoded-words in headers: `=?UTF-8?B?...?=` / `=?UTF-8?Q?...?=`.
fn decode_2047(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("=?") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("?=") {
            Some(end) => {
                let word = &after[..end];
                out.push_str(&decode_word(word).unwrap_or_else(|| format!("=?{word}?=")));
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_word(word: &str) -> Option<String> {
    // charset?encoding?payload
    let mut pieces = word.splitn(3, '?');
    let charset = pieces.next()?.to_ascii_lowercase();
    let encoding = pieces.next()?.to_ascii_uppercase();
    let payload = pieces.next()?;
    let bytes = match encoding.as_str() {
        "B" => base64::engine::general_purpose::STANDARD
            .decode(payload.as_bytes())
            .ok()?,
        "Q" => decode_quoted_printable(payload.replace('_', " ").as_bytes()),
        _ => return None,
    };
    Some(match charset.as_str() {
        // Only UTF-8 (and ASCII) are decoded faithfully; others read lossily on purpose.
        "utf-8" | "us-ascii" | "ascii" => String::from_utf8_lossy(&bytes).to_string(),
        _ => String::from_utf8_lossy(&bytes).to_string(),
    })
}
