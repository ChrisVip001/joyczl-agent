#!/usr/bin/env python3
"""假的回调接收方，只给 smoke-business.sh 的异步 hook 用。

绑定 127.0.0.1:0，把端口打印到 stdout 一行；每收到一个 POST，就把请求体追加到
输出文件（一行一条 JSON），回 200 `{"ok":true}`；收满 <接收次数> 次后自己退出。

    fake_callback_server.py <输出文件> <接收次数>

它不解释内容 —— 「回调有没有真的送到」是冒烟要回答的问题，内容由读文件的一方断言。
"""

import socket
import sys


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
