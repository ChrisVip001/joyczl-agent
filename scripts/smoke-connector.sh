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

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

fail() { echo "✗ $1" >&2; exit 1; }

need_bin

mkdir -p "$WORK/.joy"

# ---- 1. No config: nothing is enabled, and the CLI says so --------------------------
OUT="$(JOY_HOME="$WORK/.joy" "$BIN" connector list 2>&1)" || fail "connector list failed: $OUT"
grep -q "No connectors yet" <<<"$OUT" || fail "with no config it has to say so: $OUT"
echo "  ✓ no config, no connector enabled"

# ---- 2. Allowlist is a hard boundary ------------------------------------------------
python3 "$REPO/scripts/fake_smtp_server.py" "$WORK/data.txt" >"$WORK/port" &
SMTP_PID=$!
track_pid "$SMTP_PID"
for _ in $(seq 1 50); do
  [[ -s "$WORK/port" ]] && break
  sleep 0.1
done
PORT="$(head -n1 "$WORK/port" 2>/dev/null || true)"
[[ -n "$PORT" ]] || fail "the fake SMTP endpoint did not come up"

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
[[ $CODE -ne 0 ]] || fail "outside the allowlist this should fail, yet it succeeded"
grep -q "mallory@elsewhere.io" <<<"$OUT" || fail "the error has to name the recipient: $OUT"
grep -q "allow_recipients" <<<"$OUT" || fail "the error has to say where to change it: $OUT"
echo "  ✓ outside the allowlist: refused, and it says where the allowlist is"

# ---- 3. Draft mode writes outbox instead of sending --------------------------------
python3 -c "
import json, pathlib
p = pathlib.Path('$WORK/.joy/connectors.json')
config = json.loads(p.read_text())
config['email']['live'] = False
p.write_text(json.dumps(config))
"
OUT="$(JOY_HOME="$WORK/.joy" "$BIN" connector email --to alice@corp.com --subject draft --body "hold this for now" 2>&1)"
grep -q "Draft" <<<"$OUT" || fail "with live off it should write a draft: $OUT"
ls "$WORK/.joy/outbox" | grep -q "email-" || fail "there is no draft file in the outbox"
echo "  ✓ live=false: writes the outbox only, sends nothing"

# ---- 4. Live send reaches the SMTP server with real MIME bytes ---------------------
python3 -c "
import json, pathlib
p = pathlib.Path('$WORK/.joy/connectors.json')
config = json.loads(p.read_text())
config['email']['live'] = True
p.write_text(json.dumps(config))
"

# The fake server exits after one message, so this is the only live send in the smoke.
OUT="$(JOY_HOME="$WORK/.joy" "$BIN" connector email --to alice@corp.com,bob@corp.com --subject hello --body "the body text" 2>&1)" \
  || fail "the live send failed: $OUT"
grep -q "Sent to" <<<"$OUT" || fail "it should report a successful send: $OUT"

for _ in $(seq 1 50); do
  [[ -s "$WORK/data.txt" ]] && break
  sleep 0.1
done
[[ -s "$WORK/data.txt" ]] || fail "the SMTP server received no body"
grep -q "To: alice@corp.com, bob@corp.com" "$WORK/data.txt" || fail "the recipient header is wrong"
grep -q "Subject: =?UTF-8?B?" "$WORK/data.txt" || fail "the non-ASCII subject was not RFC 2047 encoded"
grep -q "Content-Transfer-Encoding: base64" "$WORK/data.txt" || fail "the body is not base64"
echo "  ✓ live: it really reached SMTP, with the recipients, the encoded subject and a base64 body all right"

# ---- 5. Feishu: allowlist, draft, then one live message ---------------------------
python3 "$REPO/scripts/fake_feishu_server.py" "$WORK/feishu.log" >"$WORK/feishu_port" &
FEISHU_PID=$!
track_pid "$FEISHU_PID"
for _ in $(seq 1 50); do
  [[ -s "$WORK/feishu_port" ]] && break
  sleep 0.1
done
FEISHU_PORT="$(head -n1 "$WORK/feishu_port" 2>/dev/null || true)"
[[ -n "$FEISHU_PORT" ]] || fail "the fake Feishu endpoint did not come up"

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
[[ $CODE -ne 0 ]] || fail "outside the Feishu allowlist this should fail"
grep -q "ou_mallory" <<<"$OUT" || fail "the Feishu error has to name the recipient: $OUT"
echo "  ✓ outside the Feishu allowlist: refused"

OUT="$(JOY_HOME="$WORK/.joy" "$BIN" connector feishu --to ou_alice --text "hold this as a draft" --dry-run 2>&1)"
grep -q "Draft" <<<"$OUT" || fail "a dry run should write a draft: $OUT"
ls "$WORK/.joy/outbox" | grep -q "feishu-" || fail "there is no Feishu draft in the outbox"
echo "  ✓ Feishu dry run: writes the outbox only"

JOY_FEISHU_SMOKE_SECRET=s3cret JOY_HOME="$WORK/.joy" \
  "$BIN" connector feishu --to ou_alice --text "a smoke-test message" >"$WORK/feishu_out.txt" 2>&1 \
  || fail "the live Feishu send failed: $(cat "$WORK/feishu_out.txt")"
grep -q "om_fake" "$WORK/feishu_out.txt" || fail "it should report a message_id: $(cat "$WORK/feishu_out.txt")"
[[ -s "$WORK/feishu.log" ]] || fail "the fake Feishu received no message request"

python3 - "$WORK/feishu.log" <<'PY' || fail "the Feishu request body is wrong"
import json, sys
line = open(sys.argv[1], encoding="utf-8").read().strip().splitlines()[0]
request = json.loads(line)
assert "receive_id_type=open_id" in request["path"], request["path"]
assert request["auth"] == "Bearer t-fake", request["auth"]
assert request["body"]["receive_id"] == "ou_alice", request["body"]
assert request["body"]["msg_type"] == "text", request["body"]
content = json.loads(request["body"]["content"])
assert content["text"] == "a smoke-test message", content
PY
echo "  ✓ Feishu live: tenant token → text message, with receive_id and content shaped right"

# ---- 6. Inbound: a real poller over a scripted IMAP, through the shipped binary ------
# The whole chain this proves: connectors.json → poller task → IMAP protocol → state.db
# → inbox/list over JSON-RPC. Nothing is mocked between the binary and the mailbox.
#
# Two runs, as in real life: the first knows the mailbox's high-water mark (that is what
# the cursor is), the second is where new mail actually arrives.
INBOUND_HOME="$WORK/inbound"
mkdir -p "$INBOUND_HOME"
cat > "$INBOUND_HOME/connectors.json" <<'EOF_INBOUND'
{"email": {"smtp_host": "127.0.0.1", "smtp_port": 2525, "from": "joy@corp.com",
           "allow_recipients": ["@corp.com"], "live": false}}
EOF_INBOUND

# Boot once without an imap block: migrations run, no poller starts, no IMAP traffic.
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"session/list","params":{}}' \
  | JOY_HOME="$INBOUND_HOME" JOY_PROVIDER=anthropic JOY_API_KEY=test \
      "$BIN" app-server >"$WORK/inbound_boot.log" 2>&1
grep -q '"id":1' "$WORK/inbound_boot.log" || fail "the inbound scenario's first boot did not answer"

# The cursor says "we have seen up to 100"; the scripted mailbox has 101 and 102.
python3 - "$INBOUND_HOME/state.db" <<'PY' || fail "seeding the poll cursor failed"
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute("INSERT INTO connector_cursors (connector, mailbox, last_uid) VALUES ('email', 'INBOX', 100)")
con.commit()
PY

python3 - "$INBOUND_HOME/connectors.json" <<'PY'
import json, sys
path = sys.argv[1]
config = json.load(open(path))
config["email"]["imap"] = {"host": "127.0.0.1", "port": 0, "mailbox": "INBOX", "poll_secs": 30, "mode": "notify"}
json.dump(config, open(path, "w"))
PY

python3 "$REPO/scripts/fake_imap_server.py" 101 102 >"$WORK/imap_port" &
IMAP_PID=$!
track_pid "$IMAP_PID"
for _ in $(seq 1 50); do
  [[ -s "$WORK/imap_port" ]] && break
  sleep 0.1
done
IMAP_PORT="$(head -n1 "$WORK/imap_port" 2>/dev/null || true)"
[[ -n "$IMAP_PORT" ]] || fail "the fake IMAP endpoint did not come up"

# Now point the config at it (port known only after the server started).
python3 - "$INBOUND_HOME/connectors.json" "$IMAP_PORT" <<'PY'
import json, sys
path, port = sys.argv[1], sys.argv[2]
config = json.load(open(path))
config["email"]["imap"]["port"] = int(port)
config["email"]["secret"] = "env:JOY_IMAP_SMOKE_PASSWORD"
config["email"]["username"] = "joy@corp.com"
json.dump(config, open(path, "w"))
PY

mkfifo "$WORK/inbound_in"
JOY_IMAP_SMOKE_PASSWORD=s3cret JOY_HOME="$INBOUND_HOME" \
  JOY_PROVIDER=anthropic JOY_API_KEY=test \
  "$BIN" app-server <"$WORK/inbound_in" >"$WORK/inbound_out.log" 2>&1 &
INBOUND_PID=$!
track_pid "$INBOUND_PID"
exec 4>"$WORK/inbound_in" # hold stdin open: the poller is a background task

sleep 2
printf '%s\n' '{"jsonrpc":"2.0","id":9,"method":"inbox/list","params":{}}' >&4
INBOX_JSON=""
for _ in $(seq 1 50); do
  if grep -q '"id":9' "$WORK/inbound_out.log" 2>/dev/null; then
    INBOX_JSON="$(grep '"id":9' "$WORK/inbound_out.log" | head -n1)"
    break
  fi
  sleep 0.1
done
exec 4>&-
# Don't wait for EOF-driven exit here: the FIFO's write end may still be held by a
# subshell fork, and this smoke only needs the answer it already got.
kill "$INBOUND_PID" 2>/dev/null || true
wait "$INBOUND_PID" 2>/dev/null || true
kill "$IMAP_PID" 2>/dev/null || true
[[ -n "$INBOX_JSON" ]] || { cat "$WORK/inbound_out.log"; fail "inbox/list did not answer"; }

python3 - "$INBOX_JSON" <<'PY' || fail "the inbox content is wrong"
import json, sys
response = json.loads(sys.argv[1])["result"]
assert response["unseen"] == 2, response
items = response["items"]
assert len(items) == 2, items
subjects = {item["subject"] for item in items}
assert "Server weekly report" in subjects, subjects        # RFC 2047 decoded
assert "invoice" in subjects, subjects
invoice = next(item for item in items if item["subject"] == "invoice")
assert "The invoice for this month" in invoice["snippet"], invoice  # base64 decoded
assert invoice["fromAddr"] == "alice@corp.com", invoice
PY
echo "  ✓ inbound: the poller really connected over IMAP; subject and body decoded into the inbox and inbox/list reads back"

# ---- 7. A user token refreshed in the background: nobody sends a message, it renews itself ---
# The chain this proves: connectors.json says as:user → app-server starts the renewer →
# the renewer sees a token inside its lead window → refresh grant over real HTTP → the file
# is rewritten with the rotated pair.
RENEW_HOME="$WORK/renew"
mkdir -p "$RENEW_HOME/connector-auth"
python3 "$REPO/scripts/fake_feishu_server.py" "$WORK/renew.log" >"$WORK/renew_port" &
RENEW_PID=$!
track_pid "$RENEW_PID"
for _ in $(seq 1 50); do
  [[ -s "$WORK/renew_port" ]] && break
  sleep 0.1
done
RENEW_PORT="$(head -n1 "$WORK/renew_port" 2>/dev/null || true)"
[[ -n "$RENEW_PORT" ]] || fail "the fake Feishu endpoint used for the refresh did not come up"

cat > "$RENEW_HOME/connectors.json" <<EOF_RENEW
{"feishu": {"app_id": "cli_fake", "app_secret": "env:JOY_FEISHU_SMOKE_SECRET",
            "as": "user", "allow_recipients": ["ou_alice"], "live": false,
            "base_url": "http://127.0.0.1:$RENEW_PORT"}}
EOF_RENEW

# Five seconds of life left: the renewer's first glance already lands inside the lead window.
python3 - "$RENEW_HOME/connector-auth/feishu.json" <<'PY_TOKEN'
import json, sys, time
json.dump({"access_token": "u-old", "refresh_token": "r-old",
           "expires_at": int(time.time()) + 5}, open(sys.argv[1], "w"))
PY_TOKEN

# stdin stays open for a few seconds: app-server shuts down when the pipe closes, and the
# renewer lives inside it.
( printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"session/list","params":{}}'; sleep 8 ) \
  | JOY_HOME="$RENEW_HOME" JOY_PROVIDER=anthropic JOY_API_KEY=test \
      JOY_FEISHU_SMOKE_SECRET=s3cret \
      "$BIN" app-server >"$WORK/renew_out.log" 2>&1 &
RENEW_SERVER=$!
track_pid "$RENEW_SERVER"

RENEWED=""
for _ in $(seq 1 100); do
  RENEWED="$(python3 - "$RENEW_HOME/connector-auth/feishu.json" <<'PY_READ'
import json, sys
try:
    print(json.load(open(sys.argv[1])).get("access_token", ""))
except Exception:
    print("")
PY_READ
)"
  [[ "$RENEWED" == "u-renewed" ]] && break
  sleep 0.1
done
kill "$RENEW_SERVER" 2>/dev/null || true
kill "$RENEW_PID" 2>/dev/null || true

[[ "$RENEWED" == "u-renewed" ]] || { cat "$WORK/renew_out.log"; fail "the token was not refreshed in the background (got: $RENEWED)"; }
grep -q "refreshed in the background" "$WORK/renew_out.log" || { cat "$WORK/renew_out.log"; fail "the refresh left no word in the log"; }

python3 - "$WORK/renew.log" <<'PY_GRANT' || fail "the refresh request is shaped wrong"
import json, sys
lines = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8") if line.strip()]
grant = next(item for item in lines if "oauth/token" in item["path"])
assert grant["body"]["grant_type"] == "refresh_token", grant
assert grant["body"]["refresh_token"] == "r-old", grant
assert grant["body"]["client_id"] == "cli_fake", grant
PY_GRANT

python3 - "$RENEW_HOME/connector-auth/feishu.json" <<'PY_ROTATED' || fail "the rotated refresh token was not stored"
import json, sys
token = json.load(open(sys.argv[1]))
assert token["refresh_token"] == "r-renewed", token
assert token["expires_at"] > 0, token
PY_ROTATED
echo "  ✓ background refresh: with nobody sending a message the refresh grant really ran, and both the token and the rotated refresh token reached the disk"

echo "✓ connector smoke test passed"
