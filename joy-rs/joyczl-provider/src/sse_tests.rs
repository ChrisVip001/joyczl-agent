//! SSE 解析的测试。不联网 —— 直接喂构造好的字节流。

use bytes::Bytes;
use futures_util::{stream, StreamExt};

use super::data_lines;

fn bytes_stream(
    chunks: Vec<&str>,
) -> impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> {
    stream::iter(
        chunks
            .into_iter()
            .map(|c| Ok(Bytes::from(c.to_string())))
            .collect::<Vec<_>>(),
    )
}

#[tokio::test]
async fn yields_each_data_payload() {
    let out = data_lines(bytes_stream(vec![
        "event: message_start\n",
        "data: {\"type\":\"message_start\"}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\"}\n",
        "\n",
    ]))
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .map(|r| r.expect("不应出错"))
    .collect::<Vec<_>>();

    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0], "{\"type\":\"message_start\"}");
    assert_eq!(out[1], "{\"type\":\"content_block_delta\"}");
}

#[tokio::test]
async fn handles_payloads_split_across_chunks() {
    // TCP 不保证按事件切分 —— 一个 JSON 可能被切成好几段。
    let out = data_lines(bytes_stream(vec![
        "event: x\ndata: {\"a\":",
        "1}\ndata: {\"b\":2}\n\ndata: [DONE]",
        "\n",
    ]))
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .map(|r| r.expect("不应出错"))
    .collect::<Vec<_>>();

    assert_eq!(
        out,
        vec![
            "{\"a\":1}".to_string(),
            "{\"b\":2}".to_string(),
            "[DONE]".to_string()
        ]
    );
}

#[tokio::test]
async fn handles_crlf_and_missing_trailing_blank_line() {
    let out = data_lines(bytes_stream(vec![
        "data: {\"a\":1}\r\n\r\ndata: {\"b\":2}\r\n", // 最后没有空行
    ]))
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .map(|r| r.expect("不应出错"))
    .collect::<Vec<_>>();

    assert_eq!(out, vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]);
}

#[tokio::test]
async fn ignores_comments_and_event_lines() {
    let out = data_lines(bytes_stream(vec![
        ": keep-alive comment\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        ": another\n",
    ]))
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .map(|r| r.expect("不应出错"))
    .collect::<Vec<_>>();

    assert_eq!(out, vec!["{\"type\":\"ping\"}".to_string()]);
}
