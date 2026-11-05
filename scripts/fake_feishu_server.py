#!/usr/bin/env python3
"""假的飞书开放平台，只给 smoke-connector.sh 用。

起在 127.0.0.1:0，端口打印到 stdout 一行；认得两个端点：
  POST /open-apis/auth/v3/tenant_access_token/internal  → {"code":0,"tenant_access_token":"t-fake"}
  POST /open-apis/im/v1/messages                         → {"code":0,"data":{"message_id":"om_fake"}}
收到的每个请求（方法 + 路径 + 关键 body）写进 argv[1]，然后退出（一次只服务一个客户端）。

不验签、不连网：冒烟测的是「连接器 → HTTP」之间那一段全是真的。
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

LOG = sys.argv[1] if len(sys.argv) > 1 else "/tmp/feishu.log"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw)
        except json.JSONDecodeError:
            body = {"raw": raw.decode(errors="replace")}
        auth = self.headers.get("Authorization", "")

        if "tenant_access_token" in self.path:
            reply = {"code": 0, "msg": "ok", "tenant_access_token": "t-fake", "expire": 7200}
        elif "/im/v1/messages" in self.path:
            with open(LOG, "a", encoding="utf-8") as out:
                out.write(json.dumps({"path": self.path, "auth": auth, "body": body}, ensure_ascii=False))
                out.write("\n")
            reply = {"code": 0, "msg": "ok", "data": {"message_id": "om_fake"}}
        else:
            reply = {"code": 404, "msg": "not found"}

        payload = json.dumps(reply).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

        if "/im/v1/messages" in self.path:
            # One message per smoke run: exit after answering. (shutdown() would deadlock:
            # serve_forever runs in this very thread.)
            sys.stdout.flush()
            raise SystemExit(0)


def main():
    server = HTTPServer(("127.0.0.1", 0), Handler)
    print(server.server_port, flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
