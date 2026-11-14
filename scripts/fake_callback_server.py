#!/usr/bin/env python3
"""A fake callback receiver, only for smoke-business.sh's asynchronous hook.

Binds 127.0.0.1:0 and prints the port as one line on stdout; every POST appends its body to
the output file (one JSON object per line) and gets 200 `{"ok":true}`; after <count>
requests it exits on its own.

    fake_callback_server.py <output file> <how many requests>

It interprets nothing — "did the callback really arrive" is the question the smoke test
answers, and whoever reads the file does the asserting.
"""

import socket
import sys


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


def send(conn, status, payload):
    header = (
        f"HTTP/1.1 {status}\r\n"
        "Content-Type: application/json\r\n"
        f"Content-Length: {len(payload)}\r\n"
        "Connection: close\r\n\r\n"
    ).encode("ascii")
    conn.sendall(header + payload)


def main(out_path, count):
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", 0))
    server.listen(4)
    print(server.getsockname()[1], flush=True)

    with open(out_path, "w", encoding="utf-8") as out:
        while count > 0:
            conn, _ = server.accept()
            try:
                body = read_request(conn)
                out.write(body.decode("utf-8", "replace") + "\n")
                out.flush()
                send(conn, "200 OK", b'{"ok":true}')
            finally:
                conn.close()
            count -= 1


if __name__ == "__main__":
    try:
        main(sys.argv[1], int(sys.argv[2]))
    except KeyboardInterrupt:
        sys.exit(0)
