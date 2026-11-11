#!/usr/bin/env bash
# End-to-end smoke test for app-server: really start a joy app-server, feed it JSON-RPC from
# stdin, and check every kind of reply. Closer to real usage than unit tests — unit tests prove
# the state is right; this proves the "protocol + transport + state" chain works.
#
#   ./scripts/smoke.sh
#
# Requires a prior cargo build (it uses target/debug/joy directly).

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/joy-rs/target/debug/joy"
WORK="$(mktemp -d)"
FAKE_PID=""
cleanup() {
  [[ -n "$FAKE_PID" ]] && kill "$FAKE_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

if [[ ! -x "$BIN" ]]; then
  echo "✗ 找不到 $BIN，先跑：cd joy-rs && cargo build" >&2
  exit 1
fi

cd "$WORK"

# Ask several things in one session: write → retrieve → forget → config → unimplemented →
# unknown method → bad JSON. Note: no API key here, so turn/start should report PROVIDER_ERROR,
# with a message telling the user what to do — while all other methods must keep working.
OUTPUT=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"memory/remember","params":{"subject":"阿明","content":"阿明喜欢早上的会议"}}' \
  '{"jsonrpc":"2.0","id":2,"method":"memory/search","params":{"query":"早上"}}' \
  '{"jsonrpc":"2.0","id":3,"method":"memory/forget","params":{"subject":"阿明"}}' \
  '{"jsonrpc":"2.0","id":4,"method":"config/read","params":{}}' \
  '{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{"message":"hi"}}' \
  '{"jsonrpc":"2.0","id":6,"method":"nope","params":{}}' \
  '{ 坏 JSON' \
  | "$BIN" app-server)

echo "$OUTPUT" | sed 's/^/  /'

# ---- Assertions ----
fail() { echo "✗ $1" >&2; exit 1; }

echo "$OUTPUT" | grep -q '"id":1,"result":{"fact"'          || fail "memory/remember 没写进去"
echo "$OUTPUT" | grep -q '"id":2,"result":{"episodes":\[\],"facts":\[{"content":"阿明喜欢早上的会议"' \
                                                              || fail "中文检索没命中"
echo "$OUTPUT" | grep -q '"id":3,"result":{"removed":1}'     || fail "memory/forget 没删掉"
echo "$OUTPUT" | grep -q '"id":4,"result":{"config":'        || fail "config/read 没返回配置"
echo "$OUTPUT" | grep -q '"id":5,"error":{"code":-32000'     || fail "没配 key 时 turn/start 应当报 PROVIDER_ERROR"
echo "$OUTPUT" | grep -q '缺 API key'                        || fail "PROVIDER_ERROR 应当说明怎么办"
echo "$OUTPUT" | grep -q '"id":6,"error":{"code":-32601'     || fail "未知方法应当报 METHOD_NOT_FOUND"
echo "$OUTPUT" | grep -q '"code":-32700'                     || fail "坏 JSON 应当报 PARSE_ERROR"

# Memory must really be an openable file — that's the whole point of "local-first".
[[ -f "$WORK/.joy/state.db" ]] || fail "state.db 没建在 .joy/ 下"
sqlite3 "$WORK/.joy/state.db" '.tables' >/dev/null 2>&1 \
  && echo "  ✓ state.db 能被 sqlite3 直接打开" \
  || echo "  ⚠ 没装 sqlite3，跳过直接打开检查"

# Start a fake SSE endpoint and print its port. The two sections below each need a fresh port
# (it exits after serving one streaming reply).
start_fake() {
  local port_file="$1"
  python3 "$REPO/scripts/fake_sse_server.py" >"$port_file" &
  FAKE_PID=$!
  for _ in $(seq 1 50); do
    [[ -s "$port_file" ]] && break
    sleep 0.1
  done
  head -n1 "$port_file" 2>/dev/null || true
}

# ---- Streaming: connect to the local fake SSE endpoint, verify SSE → textDelta → reply ----
# No real key: model/save points a profile at the fake endpoint (keyEnv pointer); requests go out.
PORT="$(start_fake "$WORK/fake_port")"
[[ -n "$PORT" ]] || fail "假 SSE 端点没起来"

# Configure the model via the protocol itself (model/save): --env pointer key + test env var —
# no real keychain, no plaintext file; base_url points at the fake endpoint. Also runs that path.
STREAM_OUT=$(printf '%s\n' \
  "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"model/save\",\"params\":{\"id\":\"fake\",\"provider\":\"anthropic\",\"apiKey\":null,\"keyEnv\":\"JOY_FAKE_KEY\",\"baseUrl\":\"http://127.0.0.1:$PORT\"}}" \
  '{"jsonrpc":"2.0","id":2,"method":"turn/start","params":{"message":"你好"}}' \
  | JOY_FAKE_KEY=test JOY_HOME="$WORK/.joy-stream" "$BIN" app-server)

echo "$STREAM_OUT" | sed 's/^/  [stream] /'

echo "$STREAM_OUT" | grep -q '"method":"turn/notification"' || fail "流式没有通知发出"
echo "$STREAM_OUT" | grep -q '"textDelta"'                 || fail "没有 textDelta 通知"
echo "$STREAM_OUT" | grep -q '"delta":"你好"'              || fail "第一个增量不对"
echo "$STREAM_OUT" | grep -q '"reply":"你好，世界"'         || fail "最终应答不是拼起来的完整文本"
echo "$STREAM_OUT" | grep -q '"id":2,"result":{"turnId"'   || fail "turn/start 没有正常应答"
echo "  ✓ 流式：SSE → textDelta → 完整应答"

# ---- Graph front door: when enabled, the classifier must fail open and still answer --------
# Same fake endpoint: non-streaming requests always 500 — so the classifier and retrieval gate
# both "fail open", the turn falls to full_agent, and the answer matches the no-graph case.
# That's the e2e version of the rule: a broken graph may cost latency, never capability.
PORT2="$(start_fake "$WORK/fake_port2")"
[[ -n "$PORT2" ]] || fail "假 SSE 端点没起来（图那次）"

GRAPH_OUT=$(printf '%s\n' \
  "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"model/save\",\"params\":{\"id\":\"fake\",\"provider\":\"anthropic\",\"apiKey\":null,\"keyEnv\":\"JOY_FAKE_KEY\",\"baseUrl\":\"http://127.0.0.1:$PORT2\"}}" \
  '{"jsonrpc":"2.0","id":2,"method":"turn/start","params":{"message":"你好"}}' \
  | JOY_FAKE_KEY=test JOY_GRAPH_WORKFLOWS=1 JOY_HOME="$WORK/.joy-graph" "$BIN" app-server)

echo "$GRAPH_OUT" | sed 's/^/  [graph] /'

echo "$GRAPH_OUT" | grep -q '"type":"graphStarted"'       || fail "开了图却没发 graphStarted"
echo "$GRAPH_OUT" | grep -q '"node":"full_agent"'         || fail "分类器坏了就该落到 full_agent"
echo "$GRAPH_OUT" | grep -q '"type":"graphEnded"'         || fail "图没正常收尾"
echo "$GRAPH_OUT" | grep -q '"route":"full"'              || fail "meta 里该记下走的是 full"
echo "$GRAPH_OUT" | grep -q '"reply":"你好，世界"'         || fail "图开着时也该给出同样的答复"
echo "$GRAPH_OUT" | grep -q '"id":2,"result":{"turnId"'   || fail "图开着时 turn/start 也得正常应答"
echo "  ✓ 图：分类器失败开放 → full_agent → 同样的答复"

# ---- hub: multi-user single-machine end to end ---------------------------------------------
# adduser → start hub → no token 401 / wrong token 401 / right token 200,
# and that user's home is created by the hub (physical evidence of isolation).
HUB="$WORK/hub"
HUB_OUT=$("$BIN" hub adduser alice --root "$HUB")
HUB_TOKEN=$(printf '%s\n' "$HUB_OUT" | grep -E '^[0-9a-f]{64}$' | head -1)
[[ -n "$HUB_TOKEN" ]] || fail "adduser 没打出一枚 64 hex 的 token"
[[ "$(printf '%s' "$HUB_TOKEN" | wc -c)" -le 64 ]] || true
grep -q '"alice"' "$HUB/users.json" || fail "users.json 里没有 alice"

# A free port every run: a hardcoded one turns "an old process is still listening" into a red
# hub section that has nothing to do with this build.
HUB_PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
JOY_HUB_ROOT="$HUB" JOY_PORT="$HUB_PORT" "$BIN" hub >/dev/null 2>&1 &
HUB_PID=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
  sleep 0.5
  curl -s -o /dev/null "http://127.0.0.1:$HUB_PORT/api/data" && break
done

code() { curl -s -m 10 -o /dev/null -w '%{http_code}' "$@"; }
[[ "$(code "http://127.0.0.1:$HUB_PORT/api/data")" == "401" ]]          || fail "hub 无 token 该 401"
[[ "$(code -H 'Authorization: Bearer wrong' "http://127.0.0.1:$HUB_PORT/api/data")" == "401" ]] || fail "hub 错 token 该 401"
[[ "$(code -H "Authorization: Bearer $HUB_TOKEN" "http://127.0.0.1:$HUB_PORT/api/data")" == "200" ]] || fail "hub 对 token 该 200"
[[ -f "$HUB/alice/state.db" ]] || fail "alice 的 home 没被建出来（隔离的物理证据）"

# 注册登录（M2）：免鉴权注册 → token 直访 → 同设备登录同 token → 错密码 401。
REG=$(curl -s -m 10 -X POST "http://127.0.0.1:$HUB_PORT/api/auth/register" \
  -H 'content-type: application/json' \
  -d '{"username":"bob","password":"hunter2-secret","device":"phone"}')
BOB_TOKEN=$(printf '%s' "$REG" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("token",""))' 2>/dev/null)
[[ -n "$BOB_TOKEN" ]] || fail "注册响应里没有 token：$REG"
[[ "$(code -H "Authorization: Bearer $BOB_TOKEN" "http://127.0.0.1:$HUB_PORT/api/data")" == "200" ]] || fail "bob 注册的 token 该 200"
LOG=$(curl -s -m 10 -X POST "http://127.0.0.1:$HUB_PORT/api/auth/login" \
  -H 'content-type: application/json' \
  -d '{"username":"bob","password":"hunter2-secret","device":"phone"}')
LOG_TOKEN=$(printf '%s' "$LOG" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("token",""))' 2>/dev/null)
[[ "$LOG_TOKEN" == "$BOB_TOKEN" ]] || fail "同设备登录该拿同一个 token"
[[ "$(code -X POST "http://127.0.0.1:$HUB_PORT/api/auth/login" -H 'content-type: application/json' \
  -d '{"username":"bob","password":"wrong-password","device":"phone"}')" == "401" ]] || fail "错密码该 401"
[[ "$(code -X POST "http://127.0.0.1:$HUB_PORT/api/auth/register" -H 'content-type: application/json' \
  -d '{"username":"bob","password":"hunter2-secret","device":"phone"}')" == "409" ]] || fail "重名注册该 409"
[[ -f "$HUB/bob/state.db" ]] || fail "bob 的 home 没被建出来"
kill "$HUB_PID" 2>/dev/null
echo "  ✓ hub：无 token 401 / 对 token 200 / 注册登录换 token，alice 与 bob 的 home 已隔离"

echo "✓ 冒烟测试通过"
