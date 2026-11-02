#!/usr/bin/env python3
"""假的 SSE 端点，只给 smoke.sh 用。

起一个 HTTP 服务，按请求体分流：

  * 带 `"stream": true` 的调用（loop 那次）→ 回一段 Anthropic 风格的 SSE：
    两个 text_delta（"你好" / "，世界"）+ message_delta + message_stop；
  * 其它调用（检索门那次）                → 500，让门「失败开放」走过去。

绑定 127.0.0.1:0，把端口打印到 stdout 一行；服务完 argv[1] 轮流式应答就自己
退出（默认 1 轮，早点退场省得留个后台进程要 kill）。冒烟测试里每多一条链要
问一句，就把这个数加上去 —— 这几条链共用同一个假端点。
这样不需要任何真 key，也能把「SSE → textDelta」这条链跑通。
"""

import socket
import sys

SSE_BODY = (
    "event: message_start\n"
    'data: {"type":"message_start","message":{"usage":{"input_tokens":10}}}\n\n'
    "event: content_block_start\n"
    'data: {"type":"content_block_start","index":0,"content_block":{"type":"text"}}\n\n'
    "event: content_block_delta\n"
    'data: {"type":"content_block_delta","index":0,'
    '"delta":{"type":"text_delta","text":"你好"}}\n\n'
    "event: content_block_delta\n"
    'data: {"type":"content_block_delta","index":0,'
    '"delta":{"type":"text_delta","text":"，世界"}}\n\n'
    "event: message_delta\n"
    'data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},'
    '"usage":{"output_tokens":5}}\n\n'
    "event: message_stop\n"
    'data: {"type":"message_stop"}\n\n'
).encode("utf-8")


def read_request(conn):
    """读到请求体为止（Content-Length 说了算，不靠连接关闭）。"""
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = conn.recv(4096)
        if not chunk:
            return b""
        data += chunk
    head, _, body = data.partition(b"\r\n\r\n")
    length = 0
    for line in head.split(b"\r\n"):
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1])
    while len(body) < length:
        chunk = conn.recv(4096)
        if not chunk:
            break
        body += chunk
    return body


def send(conn, status, content_type, payload):
    header = (
        f"HTTP/1.1 {status}\r\n"
        f"Content-Type: {content_type}\r\n"
        f"Content-Length: {len(payload)}\r\n"
        "Connection: close\r\n\r\n"
    ).encode("ascii")
    conn.sendall(header + payload)


def main(turns):
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", 0))
    server.listen(8)
    print(server.getsockname()[1], flush=True)

    while turns > 0:
        conn, _ = server.accept()
        served_stream = False
        try:
            body = read_request(conn)
            # 检索门那次请求没有 `"stream"` 键 → 500；loop 那次有 → 回 SSE。
            if b'"stream"' in body:
                send(conn, "200 OK", "text/event-stream", SSE_BODY)
                served_stream = True
            else:
                send(
                    conn,
                    "500 Internal Server Error",
                    "application/json",
                    b'{"error":{"message":"gate unavailable"}}',
                )
        finally:
            conn.close()
        if served_stream:
            turns -= 1


if __name__ == "__main__":
    try:
        main(int(sys.argv[1]) if len(sys.argv) > 1 else 1)
    except KeyboardInterrupt:
        sys.exit(0)
