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
# It serves this many streaming replies, then exits — two questions are asked below (the
# first screen's turn, and one in a freshly created session).
python3 "$REPO/scripts/fake_sse_server.py" 2 >"$WORK/fake_port" &
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
grep -q '"/api/session/new"' "$WORK/main.js" || fail "main.js 里没有 /api/session/new"
grep -q '/api/session/meta' "$WORK/main.js" || fail "main.js 里没有 /api/session/meta"
grep -q '/api/turn/steer' "$WORK/main.js" || fail "main.js 里没有 /api/turn/steer"
grep -q '/api/turn/follow-up' "$WORK/main.js" || fail "main.js 里没有 /api/turn/follow-up"
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

# ---- 0.15: a session is a row — named, with its own working directory ---------------------
# Before 0018 "new session" created nothing: an empty conversation could not be listed, named,
# or bound to a folder. These four assertions are exactly that sentence.
CREATE_JSON="$(curl -sS -X POST "$BASE/api/session/new" -H 'content-type: application/json' \
  -d "{\"title\":\"发版检查\",\"cwd\":\"$WORK\"}")" || fail "session/new 没通"
SID="$(sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p' <<<"$CREATE_JSON" | head -n1)"
[[ -n "$SID" ]] || fail "session/new 没给出会话 id：$CREATE_JSON"

curl -sS "$BASE/api/data" >"$WORK/data-meta.json"
python3 - "$WORK/data-meta.json" "$SID" "$WORK" <<'PY' || fail "空会话没有带着名字和目录出现在列表里"
import json, sys
data = json.load(open(sys.argv[1]))
row = next((s for s in data["sessions"] if s["id"] == sys.argv[2]), None)
assert row is not None, "一句话没说，但它必须已经存在"
assert row["title"] == "发版检查", row
assert row["cwd"] == sys.argv[3], row
assert row["messages"] == 0, row
PY

# Rename, then clear: "" means "no name", and the read side falls back to the first message.
curl -sS -X POST "$BASE/api/session/update" -H 'content-type: application/json' \
  -d "{\"sessionId\":\"$SID\",\"title\":\"改名了\"}" | grep -q '"title":"改名了"' \
  || fail "改名没生效"
curl -sS -X POST "$BASE/api/session/update" -H 'content-type: application/json' \
  -d "{\"sessionId\":\"$SID\",\"title\":\"\"}" | grep -q '"title":"(空会话)"' \
  || fail "清空标题后该回退「(空会话)」"
curl -sS "$BASE/api/session/meta?sessionId=$SID" | grep -q "\"cwd\":\"$WORK\"" \
  || fail "改名把工作目录弄丢了"

# A turn in that session needs no `cwd`: the session already knows it. The reply lands there,
# and with the title cleared the list falls back to this first message — both in one shot.
curl -sS -N -X POST "$BASE/api/turn" -H 'content-type: application/json' \
  -d "{\"sessionId\":\"$SID\",\"message\":\"你好\",\"stream\":false}" --max-time 20 \
  >"$WORK/turn2.sse" || fail "没有 cwd 的一轮没跑通（会话目录该兜底）"
grep -q '"type":"turnCompleted"' "$WORK/turn2.sse" || fail "第二轮没有结束 —— 前端会一直等"

curl -sS "$BASE/api/data" >"$WORK/data-meta2.json"
python3 - "$WORK/data-meta2.json" "$SID" <<'PY' || fail "清空标题的会话没有回退到首条用户消息"
import json, sys
data = json.load(open(sys.argv[1]))
row = next((s for s in data["sessions"] if s["id"] == sys.argv[2]), None)
assert row is not None, row
assert row["title"] == "你好", row
assert row["messages"] == 2, row
PY
echo "  ✓ 会话元数据：空会话可命名/可挂目录 → 改名 → 清空回退 → 一轮不带 cwd 也落在自己的目录"

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
# One door (`/api/task`) with an action field; the rules (acceptance, budget, the state
# machine) live in app-server, so the assertions check the state machine end to end.
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
grep -q '"acceptance":\[\]' "$WORK/data3.json" || fail "没写验收条件时该是空数组"
grep -q '"spent":{"wallMs":0,"tokens":0,"tools":0}' "$WORK/data3.json" || fail "花费要如实给（0 也是事实）"
grep -q '"runCount":0' "$WORK/data3.json" || fail "还没执行过，runCount 该是 0"
grep -q '"taskArtifacts":\[\]' "$WORK/data3.json" || fail "没有交付物时该给空数组"

# 0.15: a task is its own entity — the same session may hold several, and the card shows
# the newest. (It used to refuse the second one.)
curl -sS -X POST "$BASE/api/task" \
  -H 'content-type: application/json' \
  -d '{"action":"create","sessionId":"default","title":"另一件事"}' >"$WORK/task2.json"
grep -q '"title":"另一件事"' "$WORK/task2.json" || fail "第二个任务该建得成：$(cat "$WORK/task2.json")"
grep -q '"sessionId":"default"' "$WORK/task2.json" || fail "任务行该带回会话 id"

# Acceptance conditions are normalised at creation: the card's wording and the checker's
# condition are the same sentence, so this string is a real end-to-end assertion.
curl -sS -X POST "$BASE/api/task" -H 'content-type: application/json' \
  -d "{\"action\":\"create\",\"title\":\"写一份报告\",\"cwd\":\"$WORK\",\"acceptance\":[{\"kind\":\"fileExists\",\"path\":\"report.md\"}]}" \
  >"$WORK/task3.json" || fail "带验收条件的任务没建通"
grep -q '"label":"report.md 存在"' "$WORK/task3.json" || fail "验收条件没人话标签：$(cat "$WORK/task3.json")"
grep -q '"status":"pending"' "$WORK/task3.json" || fail "带验收条件的任务也该是 pending"
# A task with no session carries **no** sessionId field at all (absent, not null): absence is
# the honest shape for "belongs to nothing".
python3 -c "import json,sys; t=json.load(open('$WORK/task3.json'))['task']; sys.exit(0 if t.get('sessionId') is None else 1)" \
  || fail "没给会话的任务不该凭空长出一个会话"

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
# Three tasks exist, and the list is a list of rows — not a per-session singleton.
TASK_COUNT="$(python3 -c "import json;print(len(json.load(open('$WORK/tasks.json'))['tasks']))")"
[[ "$TASK_COUNT" -ge 3 ]] || fail "任务列表该有 3 条（实际 $TASK_COUNT）"
echo "  ✓ 任务：建 → 首屏可见（验收/花费/执行次数都有）→ 第二个也建得成 → 暂停 → 继续 → 取消"

# The connector panel is read-only truth from connectors.json: draft mode + allowlist width.
curl -sS "$BASE/api/data?sessionId=default" >"$WORK/data4.json"
grep -q '"connectors":\[{"id":"email","live":false,"allowRecipients":2}\]' "$WORK/data4.json" \
  || fail "连接器面板数据不对：$(python3 -c "import json,sys; print(json.load(open('$WORK/data4.json')).get('connectors'))")"
echo "  ✓ 连接器：草稿模式 + 白名单 2 个，窗口如实显示"

# Sessions are hard boundaries: paging an untouched session must yield an empty page, never default's.
curl -sS "$BASE/api/session?sessionId=nobody" | grep -q '"data":\[\]' \
  || fail "空会话不该看见别人的话"
echo "  ✓ 会话之间不串"

# ---- Nothing is running now ---------------------------------------------------------------
# turn/list answers "what is happening" from the live turn table; idle must be an empty array,
# not an error and not a guess. (The non-empty side is covered in app-server's own tests.)
curl -sS "$BASE/api/turns" | grep -q '"turns":\[\]' || fail "空闲时 turn/list 该是空数组"
curl -sS "$BASE/api/turns?sessionId=default" | grep -q '"turns":\[\]' \
  || fail "按会话过滤不该出错"
echo "  ✓ turn/list：空闲时是空数组"

# ---- 0.16: steering needs a turn that is actually running --------------------------------
# With nothing in flight this must fail **and say why** — silently starting a new turn would
# turn "add to what you're doing" into "start something else" without telling anyone.
STEER_ERR="$(curl -sS -X POST "$BASE/api/turn/steer" -H 'content-type: application/json' \
  -d '{"sessionId":"default","message":"顺便看下 README"}')"
grep -q '没有在跑的回合' <<<"$STEER_ERR" || fail "空闲时 turn/steer 该明说没有在跑的回合：$STEER_ERR"
echo "  ✓ turn/steer：没有在跑的回合就说没有，不静默开新一轮"

# 「跑完再说」也需要一个能等的回合：空闲时排队等于把一句话挂在已经结束的轮次后面，
# 那比拒绝更糟 —— 人会以为它终究会跑。
FOLLOW_ERR="$(curl -sS -X POST "$BASE/api/turn/follow-up" -H 'content-type: application/json' \
  -d '{"sessionId":"default","message":"把 CHANGELOG 补上"}')"
grep -q '没有在跑的回合' <<<"$FOLLOW_ERR" \
  || fail "空闲时 turn/follow-up 该明说没有在跑的回合：$FOLLOW_ERR"
echo "  ✓ turn/follow-up：没有在跑的回合就不假装排上队"

echo "✓ 驾驶舱冒烟测试通过"
