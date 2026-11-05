#!/usr/bin/env bash
# End-to-end smoke test for the dashboard: really start `joy dashboard`, curl as the browser.
#
# The frontend's own unit tests can't prove this chain — what it really proves is the translation
# layer: four read methods → one JSON screen, turn/start → SSE, spoken messages read back. So
# instead of stubs, start a real app-server (forked by `joy dashboard`) and use
# scripts/fake_sse_server.py as provider: "model stream → notifications → SSE" runs end to end.
#
#   ./scripts/smoke-dashboard.sh
#
# Requires: a cargo build, a built frontend (just dashboard-build), python3.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/joy-rs/target/debug/joy"
ASSETS="$REPO/joy-ts/packages/dashboard/dist"
WORK="$(mktemp -d)"
FAKE_PID=""
DASH_PID=""
cleanup() {
  [[ -n "$DASH_PID" ]] && kill "$DASH_PID" 2>/dev/null || true
  [[ -n "$FAKE_PID" ]] && kill "$FAKE_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "✗ $1" >&2; exit 1; }

[[ -x "$BIN" ]] || fail "找不到 $BIN，先跑：cd joy-rs && cargo build"
[[ -f "$ASSETS/index.html" ]] || fail "前端还没构建，先跑：just dashboard-build"

# Fake SSE endpoint: completes the "SSE → full reply" chain without a real key.
# It exits after serving one streaming reply, so only one question is asked below.
python3 "$REPO/scripts/fake_sse_server.py" >"$WORK/fake_port" &
FAKE_PID=$!
for _ in $(seq 1 50); do
  [[ -s "$WORK/fake_port" ]] && break
  sleep 0.1
done
MODEL_PORT="$(head -n1 "$WORK/fake_port" 2>/dev/null || true)"
[[ -n "$MODEL_PORT" ]] || fail "假 SSE 端点没起来"

# Pick a free port for the dashboard. It scans too, but after scanning the port is hard to guess —
# so read where it actually landed straight from the startup line.
PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"

# Models and keys no longer travel via env vars (see 0.7.0): seed a models.json whose
# key_source reads JOY_API_KEY from the environment and whose base_url is the fake SSE
# endpoint. JOY_PROVIDER/JOY_API_KEY below stay — provider is still the fallback vendor.
mkdir -p "$WORK/.joy"

# One connector, draft mode: the panel must show it without the tool ever sending anything.
cat > "$WORK/.joy/connectors.json" <<'EOF_CONNECTOR'
{"email": {
  "smtp_host": "127.0.0.1", "smtp_port": 2525,
  "from": "joy@corp.com",
  "allow_recipients": ["@corp.com", "alice@other.org"],
  "live": false
}}
EOF_CONNECTOR
cat > "$WORK/.joy/models.json" <<EOF
{"active":"smoke","profiles":[{"id":"smoke","provider":"anthropic","base_url":"http://127.0.0.1:$MODEL_PORT","key_source":"env:JOY_API_KEY"}]}
EOF

JOY_PORT="$PORT" \
JOY_HOME="$WORK/.joy" \
JOY_PROVIDER=anthropic JOY_API_KEY=test JOY_BASE_URL="http://127.0.0.1:$MODEL_PORT" \
  "$BIN" dashboard >"$WORK/dash.log" 2>&1 &
DASH_PID=$!

BASE=""
for _ in $(seq 1 100); do
  # The startup line prints the URL it actually landed on (localhost or 127.0.0.1).
  BASE="$(grep -oE '(localhost|127\.0\.0\.1):[0-9]+' "$WORK/dash.log" | head -n1 || true)"
  [[ -n "$BASE" ]] && break
  kill -0 "$DASH_PID" 2>/dev/null || { cat "$WORK/dash.log"; fail "驾驶舱没起来就退了"; }
  sleep 0.1
done
[[ -n "$BASE" ]] || { cat "$WORK/dash.log"; fail "等不到启动行"; }
BASE="http://$BASE"

# ---- Static files ---------------------------------------------------------------
HEAD="$(curl -sS -D - -o "$WORK/index.html" "$BASE/")"
grep -qi '^HTTP/1.1 200' <<<"$HEAD"   || fail "首页不是 200"
grep -qi 'content-type: text/html' <<<"$HEAD" || fail "首页不是 HTML"
grep -qi 'cache-control: no-store' <<<"$HEAD" || fail "没带 no-cache —— 改了页面看不出来"
grep -q 'main.js' "$WORK/index.html"  || fail "首页没引到 main.js"

# The frontend was really built: that bundle comes from esbuild — all three endpoints must be in
# it (they live in api.ts, proving the module was actually bundled, not an empty shell).
curl -sS -o "$WORK/main.js" -w '%{http_code}' "$BASE/main.js" | grep -q '^200$' \
  || fail "main.js 拿不到"
grep -q '"/api/data"' "$WORK/main.js" || fail "main.js 里没有 /api/data"
# This one matches by path only: a template string with interpolation — backticks after bundling.
grep -q '/api/session' "$WORK/main.js" || fail "main.js 里没有 /api/session"
grep -q '"/api/turn"' "$WORK/main.js" || fail "main.js 里没有 /api/turn"

# Every id the JS grabs must exist in the page — one missing means a blank screen. That error only
# shows in the browser console, invisible to curl, so pin it here. (`must("x")` is the helper in
# main.ts that "blows up on the spot"; both files are freshly built artifacts.)
grep -oE 'must\("[a-z-]+"\)' "$WORK/main.js" | grep -oE '"[a-z-]+"' | tr -d '"' | sort -u >"$WORK/want"
grep -oE 'id="[a-z-]+"' "$WORK/index.html" | grep -oE '"[a-z-]+"' | tr -d '"' | sort -u >"$WORK/have"
[[ -s "$WORK/want" ]] || fail "没能从 main.js 里认出要抓哪些 id —— 这条断言失效了，别让它静默通过"
MISSING="$(comm -23 "$WORK/want" "$WORK/have")"
[[ -z "$MISSING" ]] || fail "页面里缺这些 id：$(tr '\n' ' ' <<<"$MISSING")（浏览器会白屏）"

# Path traversal must be blocked (ServeDir is used, not hand-built paths — but this assertion is
# worth keeping: if anyone hand-writes a static file server someday, it goes red immediately).
curl -sS -o /dev/null -w '%{http_code}' --path-as-is "$BASE/../Cargo.toml" | grep -q '^404$' \
  || fail "路径穿越没被挡住"
echo "  ✓ 静态文件 + no-cache + 挡穿越"

# ---- First-screen data ---------------------------------------------------------------
curl -sS "$BASE/api/data" >"$WORK/data1.json"
grep -q '"generatedAt"' "$WORK/data1.json" || fail "首屏数据没有 generatedAt"
grep -q '"config":{' "$WORK/data1.json"    || fail "首屏数据没有配置"
grep -q '"sessions":\[\]' "$WORK/data1.json" || fail "新目录里不该有会话"
echo "  首屏：$(head -c 160 "$WORK/data1.json")"
echo "  ✓ 四个读方法 → 一屏 JSON"

# ---- Say one thing: turn/start → SSE ----------------------------------------------
curl -sS -N -X POST "$BASE/api/turn" \
  -H 'content-type: application/json' \
  -d '{"sessionId":"default","message":"你好","stream":true}' \
  --max-time 20 >"$WORK/turn.sse" || fail "POST /api/turn 没通"

echo "  这一轮收到："
sed 's/^/    /' "$WORK/turn.sse" | head -n 12

grep -q '"type":"turnStarted"'        "$WORK/turn.sse" || fail "没有 turnStarted 通知"
grep -q '"type":"textDelta"'          "$WORK/turn.sse" || fail "没有 textDelta —— 逐字就没了"
grep -q '"delta":"你好"'              "$WORK/turn.sse" || fail "增量内容不对"
grep -q '"reply":"你好，世界"'        "$WORK/turn.sse" || fail "结尾的完整答复不对"
grep -q '"type":"turnCompleted"'      "$WORK/turn.sse" || fail "没有 turnCompleted —— 前端会一直转"
echo "  ✓ turn/start → SSE 帧（浏览器解析的就是这几行）"

# ---- After speaking, the panel has content ------------------------------------------------
# That's "dashboard" vs "reading logs": the message must become something visible on screen one.
curl -sS "$BASE/api/data" >"$WORK/data2.json"
grep -q '"sessions":\[\]' "$WORK/data2.json" && fail "说完一句，会话列表还是空的"
echo "  ✓ 说过的那一句出现在首屏里"

# ---- Spoken messages must read back ------------------------------------------------------
# That's why `/api/session` exists: after a refresh the conversation is still there, and after
# switching away and back too. The protocol gives **newest first** (easy to page earlier), and
# with only two entries here, `nextCursor` should be null — "bottomed out".
curl -sS "$BASE/api/session?sessionId=default" >"$WORK/session.json"
grep -q '"role":"user"'      "$WORK/session.json" || fail "历史里没有自己那一句"
grep -q '"role":"assistant"' "$WORK/session.json" || fail "历史里没有助手那一条"
grep -q '你好，世界'          "$WORK/session.json" || fail "历史里的答复不对"
grep -q '"nextCursor":null'  "$WORK/session.json" || fail "只有两条，不该有更早的游标"
echo "  历史：$(head -c 220 "$WORK/session.json")"
echo "  ✓ 说过的话读得回来（最新在前 + 已经到底）"

# ---- Long task: create → appears on screen → pause → resume → cancel ----------------
# One door (`/api/task`) with an action field; the rules (one active task per session)
# live in app-server, so the assertions check the state machine end to end.
curl -sS -X POST "$BASE/api/task" \
  -H 'content-type: application/json' \
  -d '{"action":"create","sessionId":"default","title":"盯三天服务器","goalCondition":"三天都盯着"}' \
  >"$WORK/task1.json" || fail "建任务没通"
grep -q '"status":"pending"' "$WORK/task1.json" || fail "新任务该是 pending：$(cat "$WORK/task1.json")"
TASK_ID="$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' "$WORK/task1.json" | head -n1)"
[[ -n "$TASK_ID" ]] || fail "应答里没有任务 id"

curl -sS "$BASE/api/data?sessionId=default" >"$WORK/data3.json"
grep -q '"title":"盯三天服务器"' "$WORK/data3.json" || fail "任务没进首屏数据（任务卡会空）"
grep -q '"goalCondition":"三天都盯着"' "$WORK/data3.json" || fail "目标没进任务行"

# A second active task must be refused with the first one named — not a stack of ghosts.
curl -sS -X POST "$BASE/api/task" \
  -H 'content-type: application/json' \
  -d '{"action":"create","sessionId":"default","title":"另一件事"}' >"$WORK/task2.json"
grep -q '盯三天服务器' "$WORK/task2.json" || fail "第二个任务该被拒，并说清是哪个占着"

curl -sS -X POST "$BASE/api/task" -H 'content-type: application/json' \
  -d "{\"action\":\"suspend\",\"taskId\":\"$TASK_ID\"}" >"$WORK/suspend.json"
grep -q '"status":"suspended"' "$WORK/suspend.json" || fail "暂停没生效"

curl -sS -X POST "$BASE/api/task" -H 'content-type: application/json' \
  -d "{\"action\":\"resume\",\"taskId\":\"$TASK_ID\"}" >"$WORK/resume.json"
grep -q '"status":"running"' "$WORK/resume.json" || fail "继续没生效"

curl -sS -X POST "$BASE/api/task" -H 'content-type: application/json' \
  -d "{\"action\":\"cancel\",\"taskId\":\"$TASK_ID\"}" >"$WORK/cancel.json"
grep -q '"status":"cancelled"' "$WORK/cancel.json" || fail "取消失败"

curl -sS "$BASE/api/tasks" >"$WORK/tasks.json"
grep -q '"tasks":\[' "$WORK/tasks.json" || fail "任务列表没有 tasks 数组"
echo "  ✓ 任务：建 → 首屏可见 → 拒第二个 → 暂停 → 继续 → 取消"

# The connector panel is read-only truth from connectors.json: draft mode + allowlist width.
curl -sS "$BASE/api/data?sessionId=default" >"$WORK/data4.json"
grep -q '"connectors":\[{"id":"email","live":false,"allowRecipients":2}\]' "$WORK/data4.json" \
  || fail "连接器面板数据不对：$(python3 -c "import json,sys; print(json.load(open('$WORK/data4.json')).get('connectors'))")"
echo "  ✓ 连接器：草稿模式 + 白名单 2 个，窗口如实显示"

# Sessions are hard boundaries: paging an untouched session must yield an empty page, never default's.
curl -sS "$BASE/api/session?sessionId=nobody" | grep -q '"data":\[\]' \
  || fail "空会话不该看见别人的话"
echo "  ✓ 会话之间不串"

echo "✓ 驾驶舱冒烟测试通过"
