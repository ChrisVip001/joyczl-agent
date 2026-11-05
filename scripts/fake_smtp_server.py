#!/usr/bin/env python3
"""假的 SMTP 服务器，只给 smoke-connector.sh 用。

起在 127.0.0.1:0，把端口打印到 stdout 一行；按 SMTP 剧本应答，把 DATA 之后收到的
全部内容写进 argv[1] 指定的文件，然后自己退出（一次只服务一封信）。

不验签、不真发、不连网 —— 冒烟测的是「连接器 → socket」之间那一段全是真的，
远端这一端是可预期的假货。
"""

import socket
import sys

DATA_OUT = sys.argv[1] if len(sys.argv) > 1 else "/dev/null"


def main():
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", 0))
    server.listen(4)
    print(server.getsockname()[1], flush=True)

    conn, _ = server.accept()
    reader = conn.makefile("rb")

    def say(line):
        conn.sendall(line.encode() + b"\r\n")

    say("220 fake-smtp ready")
    while True:
        raw = reader.readline()
        if not raw:
            break
        line = raw.decode(errors="replace").rstrip("\r\n")
        if line.startswith("EHLO"):
            conn.sendall(b"250-fake\r\n250 8BITMIME\r\n")
        elif line.startswith("MAIL FROM"):
            say("250 ok")
        elif line.startswith("RCPT TO"):
            say("250 ok")
        elif line == "DATA":
            say("354 go ahead")
            payload = b""
            while True:
                chunk = reader.readline()
                if not chunk or chunk in (b".\r\n", b".\n"):
                    break
                payload += chunk
            with open(DATA_OUT, "wb") as out:
                out.write(payload)
            say("250 queued")
        elif line == "QUIT":
            say("221 bye")
            break
        else:
            say("250 ok")
    conn.close()
    server.close()


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(0)
