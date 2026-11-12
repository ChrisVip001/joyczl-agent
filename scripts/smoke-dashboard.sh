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

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ASSETS="$REPO/joy-ts/packages/dashboard/dist"

fail() { echo "✗ $1" >&2; exit 1; }

need_bin
[[ -f "$ASSETS/index.html" ]] || fail "前端还没构建，先跑：just dashboard-build"

# Fake SSE endpoint: completes the "SSE → full reply" chain without a real key.
# It serves this many streaming replies, then exits — two questions are asked below (the
# first screen's turn, and one in a freshly created session).
python3 "$REPO/scripts/fake_sse_server.py" 2 >"$WORK/fake_port" &
FAKE_PID=$!
track_pid "$FAKE_PID"
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

# 能力中心的夹具：一个用户技能 + 一个内置技能包（JOY_SKILL_DIRS）+ 一个内置 MCP
# 配置（JOY_MCP_CONFIG）。内置的在页面上置灰 —— 服务端的栅栏是「写操作一律拒绝」，
# 下面逐条验。内置包里那个服务器连不上不要紧：启动只警告，Joy 照起（容错契约）。
mkdir -p "$WORK/.joy/skills/my-skill" "$WORK/pack/skills/builtin-skill"
cat > "$WORK/.joy/skills/my-skill/SKILL.md" <<'EOF_SKILL'
---
name: my-skill
description: a skill the human installed themselves
---
body
EOF_SKILL
cat > "$WORK/pack/skills/builtin-skill/SKILL.md" <<'EOF_SKILL'
---
name: builtin-skill
description: ships with the install, read-only in the UI
---
body
EOF_SKILL
cat > "$WORK/pack/mcp.json" <<'EOF_MCP'
{"servers": [{"name": "pack-server", "url": "https://pack.example/mcp"}]}
EOF_MCP
cat > "$WORK/.joy/models.json" <<EOF
{"active":"smoke","profiles":[{"id":"smoke","provider":"anthropic","base_url":"http://127.0.0.1:$MODEL_PORT","key_source":"env:JOY_API_KEY"}]}
EOF

JOY_PORT="$PORT" \
JOY_HOME="$WORK/.joy" \
JOY_SKILL_DIRS="$WORK/pack/skills" \
JOY_MCP_CONFIG="$WORK/pack/mcp.json" \
JOY_PROVIDER=anthropic JOY_API_KEY=test JOY_BASE_URL="http://127.0.0.1:$MODEL_PORT" \
  "$BIN" dashboard >"$WORK/dash.log" 2>&1 &
DASH_PID=$!
track_pid "$DASH_PID"

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

# The stylesheet must keep the **global** `[hidden]` rule. `display: flex` beats the UA's
# `[hidden] { display: none }`, and two components in this file set `display: flex` on elements
# that are toggled with `hidden`. When the rule went missing, the memory center's three views all
# rendered at once and every segment appeared to show the ledger — the data was fine, the picture
# was not. A CSS assertion is a blunt instrument, but this failure mode is invisible to every
# other check here.
curl -sS -o "$WORK/style.css" -w '%{http_code}' "$BASE/style.css" | grep -q '^200$' \
  || fail "style.css 拿不到"
grep -qz '\[hidden\] {[[:space:]]*display: none !important;' "$WORK/style.css" \
  || fail "style.css 缺全局 [hidden] 规则 —— 用 hidden 切换的视图会同时显示"

# The page names its bundle with a build stamp. index.html is network-first and main.js is
# cache-first, so without the stamp a service-worker cache pairs a new page with an old bundle —
# the page then misses ids the bundle grabs and goes blank (it happened: the whole screen, session
# list included, disappeared). The stamp changes the cache key every build, so the pair can only
# come from the same one.
BUNDLE="$(grep -oE 'main\.js\?v=[a-z0-9]+' "$WORK/index.html" | head -1)"
[[ -n "$BUNDLE" ]] || fail "首页没给 main.js 打版本戳 —— 旧缓存会把旧 bundle 配给新页面"
curl -sS -o /dev/null -w '%{http_code}' "$BASE/$BUNDLE" | grep -q '^200$' \
  || fail "带版本戳的 $BUNDLE 拿不到（构建没打戳，或者静态服务不认 query）"
grep -q '"/api/data"' "$WORK/main.js" || fail "main.js 里没有 /api/data"
# This one matches by path only: a template string with interpolation — backticks after bundling.
grep -q '/api/session' "$WORK/main.js" || fail "main.js 里没有 /api/session"
grep -q '"/api/session/new"' "$WORK/main.js" || fail "main.js 里没有 /api/session/new"
grep -q '"/api/session/fork"' "$WORK/main.js" || fail "main.js 里没有 /api/session/fork"
grep -q '/api/session/meta' "$WORK/main.js" || fail "main.js 里没有 /api/session/meta"
grep -q '/api/turn/steer' "$WORK/main.js" || fail "main.js 里没有 /api/turn/steer"
grep -q '/api/turn/follow-up' "$WORK/main.js" || fail "main.js 里没有 /api/turn/follow-up"
grep -q '"/api/turn"' "$WORK/main.js" || fail "main.js 里没有 /api/turn"
# 能力中心的六个端点同样要在包里（少一个 = 那个按钮点了没反应）
grep -q '"/api/skills"' "$WORK/main.js" || fail "main.js 里没有 /api/skills"
grep -q '"/api/skills/install"' "$WORK/main.js" || fail "main.js 里没有 /api/skills/install"
grep -q '"/api/skills/remove"' "$WORK/main.js" || fail "main.js 里没有 /api/skills/remove"
grep -q '"/api/mcp"' "$WORK/main.js" || fail "main.js 里没有 /api/mcp"
grep -q '"/api/mcp/save"' "$WORK/main.js" || fail "main.js 里没有 /api/mcp/save"
grep -q '"/api/mcp/remove"' "$WORK/main.js" || fail "main.js 里没有 /api/mcp/remove"
# 记忆中心的六个只读端点 + 一个手动压缩：少一个 = 那一页点开是空的。
grep -q '/api/ledger/turns' "$WORK/main.js" || fail "main.js 里没有 /api/ledger/turns"
grep -q '/api/ledger/turn' "$WORK/main.js" || fail "main.js 里没有 /api/ledger/turn"
grep -q '"/api/memory/index"' "$WORK/main.js" || fail "main.js 里没有 /api/memory/index"
grep -q '/api/memory/page' "$WORK/main.js" || fail "main.js 里没有 /api/memory/page"
grep -q '/api/context' "$WORK/main.js" || fail "main.js 里没有 /api/context"
grep -q '"/api/skills/stats"' "$WORK/main.js" || fail "main.js 里没有 /api/skills/stats"
grep -q '/api/compact' "$WORK/main.js" || fail "main.js 里没有 /api/compact"
# 刷新要回到同一个会话：这个字符串没了，说明「记住当前会话」被谁顺手删了，
# 而它不会让任何 Rust 测试变红 —— 只会让人每次刷新都掉回最老的那个会话。
grep -q 'joy.session' "$WORK/main.js" || fail "main.js 里没有 joy.session（刷新会丢掉当前会话）"
grep -q '/api/schedules/jobs' "$WORK/main.js" || fail "main.js 里没有 /api/schedules/jobs"
grep -q '/api/schedules/save' "$WORK/main.js" || fail "main.js 里没有 /api/schedules/save"
grep -q '/api/schedules/remove' "$WORK/main.js" || fail "main.js 里没有 /api/schedules/remove"
# 审批卡片的四档回答：三档作用域各一个按钮。少一个，人要么只能每次都点、要么只能全放开。
# 断言的是 ASCII 的 settle(...) 调用（esbuild 会把中文转成 \uXXXX 转义，按中文 grep 会永远不中）。
grep -q 'settle(true, "once")' "$WORK/main.js" || fail "main.js 里没有「只允许这次」"
grep -q 'settle(true, "session")' "$WORK/main.js" || fail "main.js 里没有「本会话总是允许」"
grep -q 'settle(true, "always")' "$WORK/main.js" || fail "main.js 里没有「记住这条」"
# 长命令的输出要**跑着就能看见**：toolDelta 通知必须接到时间线那一行上（P0-3）。
# 少了它，工具行只会在整条命令跑完之后才出现。
grep -q 'toolDelta' "$WORK/main.js" || fail "main.js 里没有处理 toolDelta（长命令的输出不会逐段出现）"

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
  -d "{\"sessionId\":\"$SID\",\"title\":\"\"}" | grep -q '"title":"(empty session)"' \
  || fail "清空标题后该回退「(空会话)」"
curl -sS "$BASE/api/session/meta?sessionId=$SID" | grep -q "\"cwd\":\"$WORK\"" \
  || fail "改名把工作目录弄丢了"

# A turn in that session needs no `cwd`: the session already knows it. The reply lands there,
# and with the title cleared the list falls back to this first message — both in one shot.
curl -sS -N -X POST "$BASE/api/turn" -H 'content-type: application/json' \
  -d "{\"sessionId\":\"$SID\",\"message\":\"你好\",\"stream\":false}" --max-time 20 \
  >"$WORK/turn2.sse" || fail "没有 cwd 的一轮没跑通（会话目录该兜底）"
grep -q '"type":"turnCompleted"' "$WORK/turn2.sse" || fail "第二轮没有结束 —— 前端会一直等"

# ---- 0.17：分叉 —— 复制最近几轮到新会话，原会话一行不动 ---------------------------
messages_of() {
  curl -sS "$BASE/api/data" | python3 -c '
import json, sys
data = json.load(sys.stdin)
print(next(s["messages"] for s in data["sessions"] if s["id"] == sys.argv[1]))
' "$1"
}
BEFORE="$(messages_of "$SID")"
FORK_JSON="$(curl -sS -X POST "$BASE/api/session/fork" -H 'content-type: application/json' \
  -d "{\"sessionId\":\"$SID\"}")" || fail "session/fork 没通"
grep -q '"copied":[1-9]' <<<"$FORK_JSON" || fail "分叉没复制任何消息：$FORK_JSON"
grep -q "\"forkedFrom\":\"$SID\"" <<<"$FORK_JSON" || fail "分叉没记下来源：$FORK_JSON"
grep -q "\"cwd\":\"$WORK\"" <<<"$FORK_JSON" || fail "分叉丢了工作目录：$FORK_JSON"
FORK_ID="$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' <<<"$FORK_JSON" | head -n1)"
[[ -n "$FORK_ID" && "$FORK_ID" != "$SID" ]] || fail "分叉给出了坏 id：$FORK_JSON"
AFTER="$(messages_of "$SID")"
[[ "$BEFORE" == "$AFTER" ]] || fail "分叉改动了原会话（$BEFORE → $AFTER）"

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
grep -q '"label":"report.md exists"' "$WORK/task3.json" || fail "验收条件没人话标签：$(cat "$WORK/task3.json")"
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

# ---- 能力中心：技能 / MCP 连接器 -------------------------------------------------
# Built-in rows are greyed out in the page; the fence that matters is the server's:
# every write refuses them — the user's own files are the only writable ones.
curl -sS "$BASE/api/skills" >"$WORK/skills.json"
python3 - "$WORK/skills.json" <<'PY' || fail "技能列表不对：$(cat "$WORK/skills.json")"
import json, sys
rows = json.load(open(sys.argv[1]))["skills"]
by_name = {row["name"]: row for row in rows}
assert set(by_name) == {"my-skill", "builtin-skill"}, rows
assert by_name["my-skill"]["source"] == "user", rows
assert by_name["builtin-skill"]["source"] == "builtin", rows
assert by_name["my-skill"]["allowModelInvocation"] is True, rows
assert by_name["my-skill"]["folder"].endswith("skills/my-skill"), rows
PY

# 安装：粘贴一份 SKILL.md（和 joy skill install 同一道校验门）
curl -sS -X POST "$BASE/api/skills/install" -H 'content-type: application/json' \
  -d '{"text":"---\nname: pasted-skill\ndescription: pasted from the dashboard\n---\n\nbody\n"}' \
  >"$WORK/skill-install.json"
grep -q '"ok":true' "$WORK/skill-install.json" \
  || fail "粘贴安装没成功：$(cat "$WORK/skill-install.json")"
curl -sS "$BASE/api/skills" | grep -q '"name":"pasted-skill"' \
  || fail "装上的技能没出现在列表里"
# 重名拒绝（从不覆盖 —— 技能是指令，装之前要先读）
curl -sS -X POST "$BASE/api/skills/install" -H 'content-type: application/json' \
  -d '{"text":"---\nname: pasted-skill\ndescription: another one\n---\n\nbody\n"}' \
  | grep -q 'already exists' || fail "重名技能该被拒绝"
# 不是 SKILL.md 的拒绝
curl -sS -X POST "$BASE/api/skills/install" -H 'content-type: application/json' \
  -d '{"text":"just some text"}' | grep -q 'not a valid skill' || fail "非法内容该被拒绝"
# 内置技能删不得；用户技能删得掉
curl -sS -X POST "$BASE/api/skills/remove" -H 'content-type: application/json' \
  -d '{"name":"builtin-skill"}' | grep -q 'built-in' || fail "内置技能该被拒绝删除"
curl -sS -X POST "$BASE/api/skills/remove" -H 'content-type: application/json' \
  -d '{"name":"pasted-skill"}' >"$WORK/skill-remove.json"
grep -q '"ok":true' "$WORK/skill-remove.json" || fail "用户技能删不掉：$(cat "$WORK/skill-remove.json")"
curl -sS "$BASE/api/skills" | grep -q '"name":"pasted-skill"' && fail "删掉的技能还在列表里"

curl -sS "$BASE/api/mcp" >"$WORK/mcp.json"
python3 - "$WORK/mcp.json" <<'PY' || fail "MCP 列表不对：$(cat "$WORK/mcp.json")"
import json, sys
rows = json.load(open(sys.argv[1]))["servers"]
assert len(rows) == 1, rows
assert rows[0]["name"] == "pack-server", rows
assert rows[0]["source"] == "builtin", rows
assert rows[0]["kind"] == "url", rows
assert rows[0]["auth"] == "none", rows
PY

# 添加：落进 <home>/mcp.json；两种传输都填的拒绝（和服务端 resolve 同一道门）
curl -sS -X POST "$BASE/api/mcp/save" -H 'content-type: application/json' \
  -d '{"name":"notes","url":"https://notes.example/mcp","authEnv":"NOTES_API_KEY"}' \
  >"$WORK/mcp-save.json"
grep -q '"ok":true' "$WORK/mcp-save.json" || fail "加连接器没成功：$(cat "$WORK/mcp-save.json")"
grep -q 'Effective after a restart' "$WORK/mcp-save.json" || fail "该说明重启后生效：$(cat "$WORK/mcp-save.json")"
python3 -c "import json;d=json.load(open('$WORK/.joy/mcp.json'));assert [s['name'] for s in d['servers']]==['notes'],d" \
  || fail "mcp.json 没落盘或内容不对"
curl -sS -X POST "$BASE/api/mcp/save" -H 'content-type: application/json' \
  -d '{"name":"both","url":"https://h/mcp","command":"npx"}' | grep -q 'not both' \
  || fail "两种传输都填该被拒绝"
curl -sS -X POST "$BASE/api/mcp/save" -H 'content-type: application/json' \
  -d '{"name":"my server","url":"https://h/mcp"}' | grep -q 'prefix' \
  || fail "带空格的名字该被拒绝"
# 和内置包重名的拒绝：运行时先到的赢，用户配置里再写一份也不会生效
curl -sS -X POST "$BASE/api/mcp/save" -H 'content-type: application/json' \
  -d '{"name":"pack-server","url":"https://mine.example/mcp"}' | grep -q 'built-in' \
  || fail "和内置包重名的连接器该被拒绝"
# 同名是替换不是新增；stdio 的参数照写
curl -sS -X POST "$BASE/api/mcp/save" -H 'content-type: application/json' \
  -d '{"name":"notes","url":"https://moved.example/mcp"}' | grep -q '"ok":true' \
  || fail "同名保存没成功"
curl -sS -X POST "$BASE/api/mcp/save" -H 'content-type: application/json' \
  -d '{"name":"fs","command":"npx","args":["-y","server"]}' | grep -q '"ok":true' \
  || fail "stdio 连接器没加上"
python3 -c "import json;d=json.load(open('$WORK/.joy/mcp.json'));names=[s['name'] for s in d['servers']];assert names==['notes','fs'],names" \
  || fail "同名该替换而不是新增"
curl -sS "$BASE/api/mcp" >"$WORK/mcp2.json"
python3 - "$WORK/mcp2.json" <<'PY' || fail "保存后的 MCP 列表不对：$(cat "$WORK/mcp2.json")"
import json, sys
rows = {row["name"]: row for row in json.load(open(sys.argv[1]))["servers"]}
assert set(rows) == {"pack-server", "notes", "fs"}, rows
assert rows["pack-server"]["source"] == "builtin", rows
assert rows["notes"]["source"] == "user", rows
assert rows["notes"]["url"] == "https://moved.example/mcp", rows
assert rows["fs"]["kind"] == "command", rows
assert rows["fs"]["args"] == ["-y", "server"], rows
PY
# 用户连接器删得掉；内置配置里的删不到，也不该删
curl -sS -X POST "$BASE/api/mcp/remove" -H 'content-type: application/json' \
  -d '{"name":"notes"}' | grep -q '"ok":true' || fail "用户连接器删不掉"
curl -sS -X POST "$BASE/api/mcp/remove" -H 'content-type: application/json' \
  -d '{"name":"pack-server"}' | grep -q 'built-in' || fail "内置连接器该被拒绝删除"
curl -sS "$BASE/api/mcp" | grep -q '"name":"notes"' && fail "删掉的连接器还在列表里"
curl -sS "$BASE/api/mcp" | grep -q '"name":"pack-server"' || fail "内置连接器该还在"
echo "  ✓ 能力中心：内置置灰拒写，用户技能/连接器可增可删，重名与非法输入都有说法"

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
grep -q 'has no turn running' <<<"$STEER_ERR" || fail "空闲时 turn/steer 该明说没有在跑的回合：$STEER_ERR"
echo "  ✓ turn/steer：没有在跑的回合就说没有，不静默开新一轮"

# 「跑完再说」也需要一个能等的回合：空闲时排队等于把一句话挂在已经结束的轮次后面，
# 那比拒绝更糟 —— 人会以为它终究会跑。
FOLLOW_ERR="$(curl -sS -X POST "$BASE/api/turn/follow-up" -H 'content-type: application/json' \
  -d '{"sessionId":"default","message":"把 CHANGELOG 补上"}')"
grep -q 'has no turn running' <<<"$FOLLOW_ERR" \
  || fail "空闲时 turn/follow-up 该明说没有在跑的回合：$FOLLOW_ERR"
echo "  ✓ turn/follow-up：没有在跑的回合就不假装排上队"

# ---- 记忆中心：只读窗口真的答得上来 ---------------------------------------------------
# 读端点都必须回 JSON 而不是 500。`/api/context` 在没有跑过回合时是 `usage: null` ——
# 那是「还没测量」，不是「零占用」，前端据此显示空态。
curl -sS "$BASE/api/ledger/turns?sessionId=default" >"$WORK/ledger.json" \
  || fail "GET /api/ledger/turns 没通"
grep -q '"turns"' "$WORK/ledger.json" || fail "账本该回 turns"
grep -q '"entries":\[\]' "$WORK/ledger.json" \
  || fail "没点开某一轮时不该带条目"
curl -sS "$BASE/api/memory/index" | grep -q '"catalog"' || fail "/api/memory/index 该回目录"
curl -sS "$BASE/api/memory/page?page=definitely-not-a-page" \
  | grep -q '"page":null' || fail "不存在的页该是 page:null（不假装是空页）"
curl -sS "$BASE/api/context?sessionId=default" | grep -q '"usage"' \
  || fail "/api/context 该回 usage（可以是 null）"
curl -sS "$BASE/api/skills/stats" | grep -q '"data"' || fail "/api/skills/stats 该回 data"
# 缺参数要被明确拒绝，而不是拿空字符串去查库。
curl -sS "$BASE/api/ledger/turns" | grep -q 'missing parameter sessionId' \
  || fail "缺 sessionId 该明说缺什么"
echo "  ✓ 记忆中心：账本 / 目录 / 记忆页 / 占用率 / 技能命中都答得上来"

# ---- 能力中心的定时任务：声明与状态一张表 -------------------------------------------
# 新建的是一条**声明**（schedules.json 里的一行），不是立刻触发；这里验证的是「写得进去、
# 列得出来、错得明白、删得掉」，触发归 joy schedule。
curl -sS "$BASE/api/schedules/jobs" | grep -q '"jobs"' \
  || fail "/api/schedules/jobs 该回 jobs"
printf '%s' '{"name":"smoke-digest","cron":"0 8 * * *","prompt":"写一份简报","delivery":"outbox"}' \
  >"$WORK/sched.json"
curl -sS -X POST -H 'content-type: application/json' --data @"$WORK/sched.json" \
  "$BASE/api/schedules/save" | grep -q '"ok":true' || fail "新建定时任务失败"
curl -sS "$BASE/api/schedules/jobs" | grep -q 'smoke-digest' \
  || fail "新建的任务没出现在列表里"
# 坏 cron 要被当成一句话拒绝，而不是 500 或者静默保存。
curl -sS -X POST -H 'content-type: application/json' \
  --data '{"name":"smoke-digest","cron":"0 8 * *","prompt":"x"}' \
  "$BASE/api/schedules/save" | grep -q 'five fields' || fail "cron 写错该明说五段"
curl -sS -X POST -H 'content-type: application/json' --data '{"name":"smoke-digest"}' \
  "$BASE/api/schedules/remove" | grep -q '"ok":true' || fail "删除定时任务失败"
echo "  ✓ 定时任务：新建 → 列表可见 → 拒绝坏 cron → 删除"

echo "✓ 驾驶舱冒烟测试通过"
