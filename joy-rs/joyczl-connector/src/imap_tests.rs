//! IMAP receiver tests: a scripted server (real socket, real protocol) plus MIME parsing
//! as pure functions. The point of the server half is that the *commands* are right —
//! LOGIN quoting, the literal size dance on FETCH, and "first run fetches nothing".

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use super::imap::{fetch, parse, ImapConfig};

const MESSAGE: &str = "From: =?UTF-8?B?WmhhbmcgU2Fu?= <zhangsan@corp.com>\r\n\
To: joy@corp.com\r\n\
Subject: =?UTF-8?B?dGhlIHdlZWtseSByZXBvcnQgaXMgYXR0YWNoZWQ=?=\r\n\
Date: Wed, 5 Nov 2026 09:00:00 +0800\r\n\
Message-ID: <m6@corp.com>\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: 8bit\r\n\
\r\n\
The server has been fine all week.\r\n";

/// Serve exactly one IMAP session and return the commands it saw.
async fn fake_imap() -> (u16, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
    let port = listener.local_addr().expect("the address").port();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("it connects");
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut seen = Vec::new();
        write
            .write_all(b"* OK fake imap ready\r\n")
            .await
            .expect("greeting");
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            let command = line.trim_end().to_string();
            seen.push(command.clone());
            let tag = command
                .split_whitespace()
                .next()
                .unwrap_or("a1")
                .to_string();
            if command.contains("LOGIN") {
                write
                    .write_all(format!("{tag} OK logged in\r\n").as_bytes())
                    .await
                    .ok();
            } else if command.contains("SELECT") {
                write.write_all(b"* 1 EXISTS\r\n* 0 RECENT\r\n").await.ok();
                write
                    .write_all(format!("{tag} OK [READ-WRITE] selected\r\n").as_bytes())
                    .await
                    .ok();
            } else if command.contains("UID SEARCH ALL") {
                write.write_all(b"* SEARCH 4 5 6\r\n").await.ok();
                write
                    .write_all(format!("{tag} OK search done\r\n").as_bytes())
                    .await
                    .ok();
            } else if command.contains("UID SEARCH UID") {
                write.write_all(b"* SEARCH 6\r\n").await.ok();
                write
                    .write_all(format!("{tag} OK search done\r\n").as_bytes())
                    .await
                    .ok();
            } else if command.contains("UID FETCH") {
                let announce = format!("* 1 FETCH (UID 6 BODY[] {{{}}}\r\n", MESSAGE.len());
                write.write_all(announce.as_bytes()).await.ok();
                write.write_all(MESSAGE.as_bytes()).await.ok();
                write.write_all(b")\r\n").await.ok();
                write
                    .write_all(format!("{tag} OK fetch done\r\n").as_bytes())
                    .await
                    .ok();
            } else if command.contains("LOGOUT") {
                write.write_all(b"* BYE\r\n").await.ok();
                write
                    .write_all(format!("{tag} OK bye\r\n").as_bytes())
                    .await
                    .ok();
                break;
            }
        }
        seen
    });
    (port, handle)
}

fn config(port: u16) -> ImapConfig {
    ImapConfig {
        host: "127.0.0.1".to_string(),
        port,
        username: "joy".to_string(),
        password: "p@ss \"word\"".to_string(),
        mailbox: "INBOX".to_string(),
    }
}

#[tokio::test]
async fn the_first_run_only_reads_the_high_water_mark() {
    let (port, server) = fake_imap().await;
    let outcome = fetch(&config(port), None)
        .await
        .expect("the first round should succeed");
    assert!(
        outcome.messages.is_empty(),
        "the first round does not pull history"
    );
    assert_eq!(
        outcome.server_max_uid,
        Some(6),
        "the watermark should be the biggest UID in the mailbox"
    );

    let commands = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("the server should close")
        .expect("the task");
    assert!(commands.iter().any(|c| c.contains("UID SEARCH ALL")));
    assert!(
        !commands.iter().any(|c| c.contains("FETCH")),
        "the first round should not take messages"
    );
}

#[tokio::test]
async fn only_uids_after_the_cursor_are_fetched_and_parsed() {
    let (port, _server) = fake_imap().await;
    let outcome = fetch(&config(port), Some(5))
        .await
        .expect("the increment should succeed");
    assert_eq!(outcome.server_max_uid, Some(6));
    assert_eq!(outcome.messages.len(), 1);
    let message = &outcome.messages[0];
    assert_eq!(message.uid, 6);
    assert_eq!(
        message.subject, "the weekly report is attached",
        "an RFC 2047 subject has to be decoded"
    );
    assert!(
        message.from.contains("Zhang San"),
        "the sender's name has to be decoded too: {}",
        message.from
    );
    assert!(message.got_body(), "the body was fetched");
    assert_eq!(message.message_id.as_deref(), Some("<m6@corp.com>"));
}

/// Small convenience in tests: the body's actual text (kept separate from the assertion
/// above so a failure says which half broke).
trait BodyCheck {
    fn got_body(&self) -> bool;
}

impl BodyCheck for super::imap::Incoming {
    fn got_body(&self) -> bool {
        self.body.contains("The server has been fine all week")
    }
}

#[tokio::test]
async fn a_rejected_login_names_the_tag_that_failed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
    let port = listener.local_addr().expect("the address").port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("it connects");
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        write.write_all(b"* OK hi\r\n").await.ok();
        let mut line = String::new();
        let _ = reader.read_line(&mut line).await;
        write
            .write_all(b"a1 NO [AUTHENTICATIONFAILED] invalid credentials\r\n")
            .await
            .ok();
    });

    let error = fetch(&config(port), Some(1))
        .await
        .expect_err("a wrong password should fail");
    let text = error.to_string();
    assert!(text.contains("a1"), "{text}");
    assert!(
        text.contains("AUTHENTICATIONFAILED"),
        "it has to carry the server's own words: {text}"
    );
}

// ---- MIME, pure ---------------------------------------------------------------------

#[test]
fn a_multipart_alternative_prefers_the_plain_text_part() {
    let raw = b"Subject: hi\r\nContent-Type: multipart/alternative; boundary=\"b1\"\r\n\r\n\
--b1\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>HTML <b>version</b></p>\r\n\
--b1\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nplain version\r\n\
--b1--\r\n";
    let message = parse(raw, 1);
    assert_eq!(message.body, "plain version");
}

#[test]
fn html_only_mail_is_stripped_to_text() {
    let raw = b"Subject: hi\r\nContent-Type: text/html\r\n\r\n\
<p>Hello&nbsp;<b>there</b></p><script>ignore()</script>\r\n";
    let message = parse(raw, 1);
    assert!(message.body.contains("Hello"));
    assert!(!message.body.contains('<'));
}

#[test]
fn base64_and_quoted_printable_bodies_are_decoded() {
    let base64_body = b"Subject: hi\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n";
    assert_eq!(parse(base64_body, 1).body, "hello");

    let qp_body = b"Subject: hi\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n\
hello=20world=0Asecond=20line\r\n";
    assert_eq!(parse(qp_body, 1).body, "hello world\nsecond line");
}

#[test]
fn a_message_without_a_subject_is_still_a_message() {
    let raw = b"From: a@b.c\r\n\r\nthe body\r\n";
    let message = parse(raw, 9);
    assert_eq!(message.subject, "(no subject)");
    assert_eq!(message.body, "the body");
    assert_eq!(message.from, "a@b.c");
}
