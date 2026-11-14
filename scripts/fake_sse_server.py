#!/usr/bin/env python3
"""A fake SSE endpoint, only for smoke.sh.

It runs an HTTP server and branches on the request body:

  * a call carrying `"stream": true` (the loop one) → an Anthropic-style SSE stream:
    two text_deltas ("hello" / ", world") + message_delta + message_stop;
  * any other call (the retrieval gate) → 500, so the gate takes its fail-open path.

It binds 127.0.0.1:0 and prints the port as one line on stdout; after serving argv[1] streaming
replies it exits (1 by default — leaving early beats leaving a background process to kill).
Every extra chain the smoke test asks about adds one to that number: those chains share this
one fake endpoint. This is how the SSE → textDelta chain runs through with no real key.
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
    '"delta":{"type":"text_delta","text":"hello"}}\n\n'
    "event: content_block_delta\n"
    'data: {"type":"content_block_delta","index":0,'
    '"delta":{"type":"text_delta","text":", world"}}\n\n'
    "event: message_delta\n"
    'data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},'
    '"usage":{"output_tokens":5}}\n\n'
    "event: message_stop\n"
    'data: {"type":"message_stop"}\n\n'
).encode("utf-8")


def read_request(conn):
    """Read up to the request body (Content-Length decides, not the connection closing)."""
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
            # The gate's request has no `"stream"` key → 500; the loop's does → answer with SSE.
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
