#!/usr/bin/env bash
# End-to-end smoke for the email connector: a real `joy connector email` against a
# scripted SMTP server.
#
# What only this can prove: the CLI path the human uses, the allowlist boundary as the
# user experiences it, and the MIME bytes a real server would receive. Rust tests cover
# the same seams in-process; this one runs the shipped binary.
#
#   ./scripts/smoke-connector.sh
#
# Requires: a cargo build, python3. No network, no real mail server.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/joy-rs/target/debug/joy"
WORK="$(mktemp -d)"
SMTP_PID=""
cleanup() {
  [[ -n "$SMTP_PID" ]] && kill "$SMTP_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "✗ $1" >&2; exit 1; }

[[ -x "$BIN" ]] || fail "找不到 $BIN，先跑：cd joy-rs && cargo build"

mkdir -p "$WORK/.joy"

# ---- 1. No config: nothing is enabled, and the CLI says so --------------------------
OUT="$(JOY_HOME="$WORK/.joy" "$BIN" connector list 2>&1)" || fail "connector list 失败：$OUT"
grep -q "还没有连接器" <<<"$OUT" || fail "没有配置时该说清楚：$OUT"
echo "  ✓ 没有配置时不启用任何连接器"

# ---- 2. Allowlist is a hard boundary ------------------------------------------------
python3 "$REPO/scripts/fake_smtp_server.py" "$WORK/data.txt" >"$WORK/port" &
SMTP_PID=$!
for _ in $(seq 1 50); do
  [[ -s "$WORK/port" ]] && break
  sleep 0.1
done
PORT="$(head -n1 "$WORK/port" 2>/dev/null || true)"
[[ -n "$PORT" ]] || fail "假 SMTP 端点没起来"

cat > "$WORK/.joy/connectors.json" <<EOF
{"email": {
  "smtp_host": "127.0.0.1",
  "smtp_port": $PORT,
  "from": "joy@corp.com",
  "allow_recipients": ["@corp.com"],
  "live": true
}}
EOF

set +e
OUT="$(JOY_HOME="$WORK/.joy" "$BIN" connector email --to mallory@elsewhere.io --subject s --body b 2>&1)"
CODE=$?
set -e
[[ $CODE -ne 0 ]] || fail "白名单外该失败，却成功了"
grep -q "mallory@elsewhere.io" <<<"$OUT" || fail "报错该点名收件人：$OUT"
grep -q "allow_recipients" <<<"$OUT" || fail "报错该说清去哪改：$OUT"
echo "  ✓ 白名单外：拒绝，并指明白名单在哪"

# ---- 3. Draft mode writes outbox instead of sending --------------------------------
python3 -c "
import json, pathlib
p = pathlib.Path('$WORK/.joy/connectors.json')
config = json.loads(p.read_text())
config['email']['live'] = False
p.write_text(json.dumps(config))
"
OUT="$(JOY_HOME="$WORK/.joy" "$BIN" connector email --to alice@corp.com --subject 草稿 --body 先存着 2>&1)"
grep -q "草稿" <<<"$OUT" || fail "关掉 live 后该写草稿：$OUT"
ls "$WORK/.joy/outbox" | grep -q "email-" || fail "outbox 里没有草稿文件"
echo "  ✓ live=false：只写 outbox，不发"

# ---- 4. Live send reaches the SMTP server with real MIME bytes ---------------------
python3 -c "
import json, pathlib
p = pathlib.Path('$WORK/.joy/connectors.json')
config = json.loads(p.read_text())
config['email']['live'] = True
p.write_text(json.dumps(config))
"

# The fake server exits after one message, so this is the only live send in the smoke.
OUT="$(JOY_HOME="$WORK/.joy" "$BIN" connector email --to alice@corp.com,bob@corp.com --subject 你好 --body 正文内容 2>&1)" \
  || fail "live 发送失败：$OUT"
grep -q "已发给" <<<"$OUT" || fail "该报告发送成功：$OUT"

for _ in $(seq 1 50); do
  [[ -s "$WORK/data.txt" ]] && break
  sleep 0.1
done
[[ -s "$WORK/data.txt" ]] || fail "SMTP 服务器没收到正文"
grep -q "To: alice@corp.com, bob@corp.com" "$WORK/data.txt" || fail "收件人头不对"
grep -q "Subject: =?UTF-8?B?" "$WORK/data.txt" || fail "中文主题没有按 RFC 2047 编码"
grep -q "Content-Transfer-Encoding: base64" "$WORK/data.txt" || fail "正文不是 base64"
echo "  ✓ live：真的走到 SMTP，收件人 / 主题编码 / base64 正文都对"

# ---- 5. Feishu: allowlist, draft, then one live message ---------------------------
python3 "$REPO/scripts/fake_feishu_server.py" "$WORK/feishu.log" >"$WORK/feishu_port" &
FEISHU_PID=$!
for _ in $(seq 1 50); do
  [[ -s "$WORK/feishu_port" ]] && break
  sleep 0.1
done
FEISHU_PORT="$(head -n1 "$WORK/feishu_port" 2>/dev/null || true)"
[[ -n "$FEISHU_PORT" ]] || fail "假飞书端点没起来"
trap 'kill "$SMTP_PID" "$FEISHU_PID" 2>/dev/null || true; rm -rf "$WORK"' EXIT

python3 - "$WORK/.joy/connectors.json" "$FEISHU_PORT" <<'PY'
import json, sys
path, port = sys.argv[1], sys.argv[2]
config = json.load(open(path))
config["feishu"] = {
    "app_id": "cli_smoke",
    "app_secret": "env:JOY_FEISHU_SMOKE_SECRET",
    "receive_id_type": "open_id",
    "allow_recipients": ["ou_alice"],
    "live": True,
    "base_url": f"http://127.0.0.1:{port}",
}
json.dump(config, open(path, "w"))
PY

set +e
OUT="$(JOY_HOME="$WORK/.joy" "$BIN" connector feishu --to ou_mallory --text hi 2>&1)"
CODE=$?
set -e
[[ $CODE -ne 0 ]] || fail "飞书白名单外该失败"
grep -q "ou_mallory" <<<"$OUT" || fail "飞书报错该点名接收方：$OUT"
echo "  ✓ 飞书白名单外：拒绝"

OUT="$(JOY_HOME="$WORK/.joy" "$BIN" connector feishu --to ou_alice --text 先存草稿 --dry-run 2>&1)"
grep -q "草稿" <<<"$OUT" || fail "dry-run 该写草稿：$OUT"
ls "$WORK/.joy/outbox" | grep -q "feishu-" || fail "outbox 里没有飞书草稿"
echo "  ✓ 飞书 dry-run：只写 outbox"

JOY_FEISHU_SMOKE_SECRET=s3cret JOY_HOME="$WORK/.joy" \
  "$BIN" connector feishu --to ou_alice --text 冒烟测试消息 >"$WORK/feishu_out.txt" 2>&1 \
  || fail "飞书 live 发送失败：$(cat "$WORK/feishu_out.txt")"
grep -q "om_fake" "$WORK/feishu_out.txt" || fail "该报告 message_id：$(cat "$WORK/feishu_out.txt")"
[[ -s "$WORK/feishu.log" ]] || fail "假飞书没收到消息请求"

python3 - "$WORK/feishu.log" <<'PY' || fail "飞书请求体不对"
import json, sys
line = open(sys.argv[1], encoding="utf-8").read().strip().splitlines()[0]
request = json.loads(line)
assert "receive_id_type=open_id" in request["path"], request["path"]
assert request["auth"] == "Bearer t-fake", request["auth"]
assert request["body"]["receive_id"] == "ou_alice", request["body"]
assert request["body"]["msg_type"] == "text", request["body"]
content = json.loads(request["body"]["content"])
assert content["text"] == "冒烟测试消息", content
PY
echo "  ✓ 飞书 live：租户 token → 文本消息，receive_id / content 形状都对"

echo "✓ 连接器冒烟测试通过"
