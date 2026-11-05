#!/usr/bin/env python3
"""假的 IMAP 服务器，只给 smoke-connector.sh 的入站一段用。

起在 127.0.0.1:0，端口打印到 stdout 一行；剧本覆盖 Joy 客户端真正会说的四句话：
  LOGIN / SELECT / UID SEARCH / UID FETCH + LOGOUT。
邮箱里预置两封信；UID 从参数给（默认 101、102）。服务一个会话后退出。

不验签、不连网：冒烟测的是「轮询器 → 协议 → 收件箱表」之间那一段全是真的。
"""

import sys

UID_A = int(sys.argv[1]) if len(sys.argv) > 1 else 101
UID_B = int(sys.argv[2]) if len(sys.argv) > 2 else 102

MESSAGES = {
    UID_A: (
        "From: =?UTF-8?B?5byg5LiJ?= <zhangsan@corp.com>\r\n"
        "To: joy@corp.com\r\n"
        "Subject: =?UTF-8?B?5pyN5Yqh5Zmo5ZGo5oql?=\r\n"
        "Date: Wed, 5 Nov 2026 09:00:00 +0800\r\n"
        "Message-ID: <smoke-101@corp.com>\r\n"
        "Content-Type: text/plain; charset=utf-8\r\n"
        "\r\n"
        "这周 CPU 一直低于 40%。\r\n"
    ),
    UID_B: (
        "From: alice@corp.com\r\n"
        "To: joy@corp.com\r\n"
        "Subject: 发票\r\n"
        "Date: Wed, 5 Nov 2026 10:00:00 +0800\r\n"
        "Message-ID: <smoke-102@corp.com>\r\n"
        "Content-Type: text/plain; charset=utf-8\r\n"
        "Content-Transfer-Encoding: base64\r\n"
        "\r\n"
        "5pys5pyI5Y+R56Wo5Zyo6ZmE5Lu26YeM44CC\r\n"  # 本月发票在附件里。
    ),
}


def payload_bytes(uid):
    return MESSAGES[uid].encode("utf-8")


def main():
    import socket

    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", 0))
    server.listen(4)
    print(server.getsockname()[1], flush=True)

    conn, _ = server.accept()
    reader = conn.makefile("rb")

    def say(line):
        conn.sendall(line.encode() + b"\r\n")

    say("* OK fake imap ready")
    selected = False
    while True:
        raw = reader.readline()
        if not raw:
            break
        line = raw.decode(errors="replace").rstrip("\r\n")
        tag = line.split(" ", 1)[0] if line else "a1"
        upper = line.upper()
        if " LOGIN " in f" {upper} ":
            say(f"{tag} OK logged in")
        elif " SELECT " in f" {upper} ":
            selected = True
            say("* 2 EXISTS")
            say("* 0 RECENT")
            say(f"{tag} OK [READ-WRITE] selected")
        elif "UID SEARCH ALL" in upper:
            say(f"* SEARCH {UID_A} {UID_B}")
            say(f"{tag} OK search done")
        elif "UID SEARCH UID" in upper:
            # Our client asks for "<cursor+1>:*" — answer with everything above the cursor.
            requested = int(line.split("UID")[-1].split(":")[0])
            found = [uid for uid in sorted(MESSAGES) if uid >= requested]
            say("* SEARCH " + " ".join(str(uid) for uid in found) if found else "* SEARCH")
            say(f"{tag} OK search done")
        elif "UID FETCH" in upper:
            uid = int(line.split("FETCH")[-1].split()[0])
            data = payload_bytes(uid)
            conn.sendall(f"* 1 FETCH (UID {uid} BODY[] {{{len(data)}}}\r\n".encode())
            conn.sendall(data)
            conn.sendall(b")\r\n")
            say(f"{tag} OK fetch done")
        elif "LOGOUT" in upper:
            say("* BYE")
            say(f"{tag} OK bye")
            break
        else:
            say(f"{tag} BAD unsupported")
    conn.close()
    server.close()


if __name__ == "__main__":
    main()
