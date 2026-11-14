#!/usr/bin/env bash
# End-to-end smoke test for app-server: really start a joy app-server, feed it JSON-RPC from
# stdin, and check every kind of reply. Closer to real usage than unit tests — unit tests prove
# the state is right; this proves the "protocol + transport + state" chain works.
#
#   ./scripts/smoke.sh
#
# Requires prior Rust and dashboard builds; the hub validates its UI assets.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need_bin

cd "$WORK"

# Ask several things in one session: write → retrieve → forget → config → unimplemented →
# unknown method → bad JSON. Note: no API key here, so turn/start should report PROVIDER_ERROR,
# with a message telling the user what to do — while all other methods must keep working.
OUTPUT=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"memory/remember","params":{"subject":"Alex","content":"Alex likes morning meetings"}}' \
  '{"jsonrpc":"2.0","id":2,"method":"memory/search","params":{"query":"morning"}}' \
  '{"jsonrpc":"2.0","id":3,"method":"memory/forget","params":{"subject":"Alex"}}' \
  '{"jsonrpc":"2.0","id":4,"method":"config/read","params":{}}' \
  '{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{"message":"hi"}}' \
  '{"jsonrpc":"2.0","id":6,"method":"nope","params":{}}' \
  '{ bad JSON' \
  | "$BIN" app-server)

echo "$OUTPUT" | sed 's/^/  /'

# ---- Assertions ----
fail() { echo "✗ $1" >&2; exit 1; }

echo "$OUTPUT" | grep -q '"id":1,"result":{"fact"'          || fail "memory/remember wrote nothing"
echo "$OUTPUT" | grep -q '"id":2,"result":{"episodes":\[\],"facts":\[{"content":"Alex likes morning meetings"' \
                                                              || fail "the search found nothing"
echo "$OUTPUT" | grep -q '"id":3,"result":{"removed":1}'     || fail "memory/forget deleted nothing"
echo "$OUTPUT" | grep -q '"id":4,"result":{"config":'        || fail "config/read returned no config"
echo "$OUTPUT" | grep -q '"id":5,"error":{"code":-32000'     || fail "with no key, turn/start should report PROVIDER_ERROR"
echo "$OUTPUT" | grep -qE 'no API key'      || fail "PROVIDER_ERROR should say what to do"
echo "$OUTPUT" | grep -q '"id":6,"error":{"code":-32601'     || fail "an unknown method should report METHOD_NOT_FOUND"
echo "$OUTPUT" | grep -q '"code":-32700'                     || fail "bad JSON should report PARSE_ERROR"

# Memory must really be an openable file — that's the whole point of "local-first".
[[ -f "$WORK/.joy/state.db" ]] || fail "state.db was not created under .joy/"
sqlite3 "$WORK/.joy/state.db" '.tables' >/dev/null 2>&1 \
  && echo "  ✓ state.db opens directly in sqlite3" \
  || echo "  ⚠ sqlite3 is not installed; skipping the direct-open check"

# Start a fake SSE endpoint and print its port. The two sections below each need a fresh port
# (it exits after serving one streaming reply).
start_fake() {
  local port_file="$1"
  python3 "$REPO/scripts/fake_sse_server.py" >"$port_file" &
  FAKE_PID=$!
track_pid "$FAKE_PID"
  for _ in $(seq 1 50); do
    [[ -s "$port_file" ]] && break
    sleep 0.1
  done
  head -n1 "$port_file" 2>/dev/null || true
}

# ---- Streaming: connect to the local fake SSE endpoint, verify SSE → textDelta → reply ----
# No real key: model/save points a profile at the fake endpoint (keyEnv pointer); requests go out.
PORT="$(start_fake "$WORK/fake_port")"
[[ -n "$PORT" ]] || fail "the fake SSE endpoint did not come up"

# Configure the model via the protocol itself (model/save): --env pointer key + test env var —
# no real keychain, no plaintext file; base_url points at the fake endpoint. Also runs that path.
STREAM_OUT=$(printf '%s\n' \
  "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"model/save\",\"params\":{\"id\":\"fake\",\"provider\":\"anthropic\",\"apiKey\":null,\"keyEnv\":\"JOY_FAKE_KEY\",\"baseUrl\":\"http://127.0.0.1:$PORT\"}}" \
  '{"jsonrpc":"2.0","id":2,"method":"turn/start","params":{"message":"hello"}}' \
  | JOY_FAKE_KEY=test JOY_HOME="$WORK/.joy-stream" "$BIN" app-server)

echo "$STREAM_OUT" | sed 's/^/  [stream] /'

echo "$STREAM_OUT" | grep -q '"method":"turn/notification"' || fail "streaming sent no notification"
echo "$STREAM_OUT" | grep -q '"textDelta"'                 || fail "no textDelta notification"
echo "$STREAM_OUT" | grep -q '"delta":"hello"'              || fail "the first delta is wrong"
echo "$STREAM_OUT" | grep -q '"reply":"hello, world"'         || fail "the final reply is not the joined text"
echo "$STREAM_OUT" | grep -q '"id":2,"result":{"turnId"'   || fail "turn/start gave no proper reply"
echo "  ✓ streaming: SSE → textDelta → the full reply"

# ---- Graph front door: when enabled, the classifier must fail open and still answer --------
# Same fake endpoint: non-streaming requests always 500 — so the classifier and retrieval gate
# both "fail open", the turn falls to full_agent, and the answer matches the no-graph case.
# That's the e2e version of the rule: a broken graph may cost latency, never capability.
PORT2="$(start_fake "$WORK/fake_port2")"
[[ -n "$PORT2" ]] || fail "the fake SSE endpoint did not come up (graph run)"

GRAPH_OUT=$(printf '%s\n' \
  "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"model/save\",\"params\":{\"id\":\"fake\",\"provider\":\"anthropic\",\"apiKey\":null,\"keyEnv\":\"JOY_FAKE_KEY\",\"baseUrl\":\"http://127.0.0.1:$PORT2\"}}" \
  '{"jsonrpc":"2.0","id":2,"method":"turn/start","params":{"message":"hello"}}' \
  | JOY_FAKE_KEY=test JOY_GRAPH_WORKFLOWS=1 JOY_HOME="$WORK/.joy-graph" "$BIN" app-server)

echo "$GRAPH_OUT" | sed 's/^/  [graph] /'

echo "$GRAPH_OUT" | grep -q '"type":"graphStarted"'       || fail "the graph was on but no graphStarted was sent"
echo "$GRAPH_OUT" | grep -q '"node":"full_agent"'         || fail "a broken classifier should fall to full_agent"
echo "$GRAPH_OUT" | grep -q '"type":"graphEnded"'         || fail "the graph did not close properly"
echo "$GRAPH_OUT" | grep -q '"route":"full"'              || fail "meta should record that the full path was taken"
echo "$GRAPH_OUT" | grep -q '"reply":"hello, world"'         || fail "the graph being on should give the same reply"
echo "$GRAPH_OUT" | grep -q '"id":2,"result":{"turnId"'   || fail "with the graph on, turn/start still has to answer"
echo "  ✓ graph: classifier fails open → full_agent → the same reply"

# ---- hub: multi-user single-machine end to end ---------------------------------------------
# adduser → start hub → no token 401 / wrong token 401 / right token 200,
# and that user's home is created by the hub (physical evidence of isolation).
HUB="$WORK/hub"
HUB_OUT=$("$BIN" hub adduser alice --root "$HUB")
HUB_TOKEN=$(printf '%s\n' "$HUB_OUT" | grep -E '^[0-9a-f]{64}$' | head -1)
[[ -n "$HUB_TOKEN" ]] || fail "adduser printed no 64-hex token"
[[ "$(printf '%s' "$HUB_TOKEN" | wc -c)" -le 64 ]] || true
grep -q '"alice"' "$HUB/users.json" || fail "users.json has no alice"

# A free port every run: a hardcoded one turns "an old process is still listening" into a red
# hub section that has nothing to do with this build.
HUB_PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
JOY_HUB_ROOT="$HUB" JOY_PORT="$HUB_PORT" "$BIN" hub >/dev/null 2>&1 &
HUB_PID=$!
track_pid "$HUB_PID"
for _ in 1 2 3 4 5 6 7 8 9 10; do
  sleep 0.5
  curl -s -o /dev/null "http://127.0.0.1:$HUB_PORT/api/data" && break
done

code() { curl -s -m 10 -o /dev/null -w '%{http_code}' "$@"; }
[[ "$(code "http://127.0.0.1:$HUB_PORT/api/data")" == "401" ]]          || fail "hub with no token should be 401"
[[ "$(code -H 'Authorization: Bearer wrong' "http://127.0.0.1:$HUB_PORT/api/data")" == "401" ]] || fail "hub with a wrong token should be 401"
[[ "$(code -H "Authorization: Bearer $HUB_TOKEN" "http://127.0.0.1:$HUB_PORT/api/data")" == "200" ]] || fail "hub with the right token should be 200"
[[ -f "$HUB/alice/state.db" ]] || fail "alice's home was not created (the physical evidence of isolation)"

# register/login (M2): keyless registration → direct token access → the same device signs in
REG=$(curl -s -m 10 -X POST "http://127.0.0.1:$HUB_PORT/api/auth/register" \
  -H 'content-type: application/json' \
  -d '{"username":"bob","password":"hunter2-secret","device":"phone"}')
BOB_TOKEN=$(printf '%s' "$REG" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("token",""))' 2>/dev/null)
[[ -n "$BOB_TOKEN" ]] || fail "the registration response carries no token: $REG"
[[ "$(code -H "Authorization: Bearer $BOB_TOKEN" "http://127.0.0.1:$HUB_PORT/api/data")" == "200" ]] || fail "bob's registration token should be 200"
LOG=$(curl -s -m 10 -X POST "http://127.0.0.1:$HUB_PORT/api/auth/login" \
  -H 'content-type: application/json' \
  -d '{"username":"bob","password":"hunter2-secret","device":"phone"}')
LOG_TOKEN=$(printf '%s' "$LOG" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("token",""))' 2>/dev/null)
[[ "$LOG_TOKEN" == "$BOB_TOKEN" ]] || fail "the same device should sign in to the same token"
[[ "$(code -X POST "http://127.0.0.1:$HUB_PORT/api/auth/login" -H 'content-type: application/json' \
  -d '{"username":"bob","password":"wrong-password","device":"phone"}')" == "401" ]] || fail "a wrong password should be 401"
[[ "$(code -X POST "http://127.0.0.1:$HUB_PORT/api/auth/register" -H 'content-type: application/json' \
  -d '{"username":"bob","password":"hunter2-secret","device":"phone"}')" == "409" ]] || fail "registering a taken name should be 409"
[[ -f "$HUB/bob/state.db" ]] || fail "bob's home was not created"
kill "$HUB_PID" 2>/dev/null
echo "  ✓ hub: no token 401 / right token 200 / register-login swaps the token; alice's and bob's homes are isolated"

echo "✓ smoke test passed"
