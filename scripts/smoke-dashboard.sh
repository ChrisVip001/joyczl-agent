#!/usr/bin/env bash
# 驾驶舱的端到端冒烟测试：真的起 `joy dashboard`，用 curl 当浏览器。
#
# 前端自己的单测证明不了这条链 —— 它真正要证明的是那个翻译层：
# 四个读方法 → 一屏 JSON、turn/start → SSE、说过的话再读得回来。所以这里不打桩，而是起一个真的
# app-server（`joy dashboard` 自己 fork 的），再用 scripts/fake_sse_server.py
# 当 model provider：不用真 key，也能把「模型流 → 通知 → SSE 帧」整条走完。
#
#   ./scripts/smoke-dashboard.sh
#
# 需要：cargo build 过、前端构建过（just dashboard-build）、python3。

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

# 假的 SSE 端点：不用真 key 也能把「SSE → 完整答复」这条链走完。
# 它服务完一轮流式应答就自己退场，所以下面只问一句。
python3 "$REPO/scripts/fake_sse_server.py" >"$WORK/fake_port" &
FAKE_PID=$!
for _ in $(seq 1 50); do
  [[ -s "$WORK/fake_port" ]] && break
  sleep 0.1
done
MODEL_PORT="$(head -n1 "$WORK/fake_port" 2>/dev/null || true)"
[[ -n "$MODEL_PORT" ]] || fail "假 SSE 端点没起来"

# 挑一个空端口给驾驶舱。它自己也会扫，但扫了之后端口就不好猜了 ——
# 下面直接从启动行里读它到底落在哪儿。
PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"

JOY_PORT="$PORT" \
JOY_HOME="$WORK/.joy" \
JOY_PROVIDER=anthropic JOY_API_KEY=test JOY_BASE_URL="http://127.0.0.1:$MODEL_PORT" \
  "$BIN" dashboard >"$WORK/dash.log" 2>&1 &
DASH_PID=$!

BASE=""
for _ in $(seq 1 100); do
  BASE="$(sed -n 's|.*http://\(localhost:[0-9]*\).*|\1|p' "$WORK/dash.log" | head -n1)"
  [[ -n "$BASE" ]] && break
  kill -0 "$DASH_PID" 2>/dev/null || { cat "$WORK/dash.log"; fail "驾驶舱没起来就退了"; }
  sleep 0.1
done
[[ -n "$BASE" ]] || { cat "$WORK/dash.log"; fail "等不到启动行"; }
BASE="http://$BASE"

# ---- 静态文件 ---------------------------------------------------------------
HEAD="$(curl -sS -D - -o "$WORK/index.html" "$BASE/")"
grep -qi '^HTTP/1.1 200' <<<"$HEAD"   || fail "首页不是 200"
grep -qi 'content-type: text/html' <<<"$HEAD" || fail "首页不是 HTML"
grep -qi 'cache-control: no-store' <<<"$HEAD" || fail "没带 no-cache —— 改了页面看不出来"
grep -q 'main.js' "$WORK/index.html"  || fail "首页没引到 main.js"

# 前端真的构建过：那是 esbuild 打出来的包 —— 三个端点得都在里面
# （它们在 api.ts 里，证明模块真的被合进来了，不是在发一个空壳）。
curl -sS -o "$WORK/main.js" -w '%{http_code}' "$BASE/main.js" | grep -q '^200$' \
  || fail "main.js 拿不到"
grep -q '"/api/data"' "$WORK/main.js" || fail "main.js 里没有 /api/data"
# 这条只认路径：它是带插值的模板串，打包后两边是反引号，双引号那条 grep 挂不上。
grep -q '/api/session' "$WORK/main.js" || fail "main.js 里没有 /api/session"
grep -q '"/api/turn"' "$WORK/main.js" || fail "main.js 里没有 /api/turn"

# JS 要抓的每个 id 都得在页面里 —— 少一个就是白屏。这个错误只在浏览器
# 控制台里，curl 看不见，所以在这儿钉住。（`must("x")` 是 main.ts 里
# 「找不到就当场炸」的那个助手，两个文件都是刚拿到手的构建产物。）
grep -oE 'must\("[a-z-]+"\)' "$WORK/main.js" | grep -oE '"[a-z-]+"' | tr -d '"' | sort -u >"$WORK/want"
grep -oE 'id="[a-z-]+"' "$WORK/index.html" | grep -oE '"[a-z-]+"' | tr -d '"' | sort -u >"$WORK/have"
[[ -s "$WORK/want" ]] || fail "没能从 main.js 里认出要抓哪些 id —— 这条断言失效了，别让它静默通过"
MISSING="$(comm -23 "$WORK/want" "$WORK/have")"
[[ -z "$MISSING" ]] || fail "页面里缺这些 id：$(tr '\n' ' ' <<<"$MISSING")（浏览器会白屏）"

# 路径穿越得被挡住（用的是 ServeDir，不是自己拼路径 —— 但这条断言值得留着，
# 因为哪天有人手写静态文件服务，它会立刻红）。
curl -sS -o /dev/null -w '%{http_code}' --path-as-is "$BASE/../Cargo.toml" | grep -q '^404$' \
  || fail "路径穿越没被挡住"
echo "  ✓ 静态文件 + no-cache + 挡穿越"

# ---- 首屏数据 ---------------------------------------------------------------
curl -sS "$BASE/api/data" >"$WORK/data1.json"
grep -q '"generatedAt"' "$WORK/data1.json" || fail "首屏数据没有 generatedAt"
grep -q '"config":{' "$WORK/data1.json"    || fail "首屏数据没有配置"
grep -q '"sessions":\[\]' "$WORK/data1.json" || fail "新目录里不该有会话"
echo "  首屏：$(head -c 160 "$WORK/data1.json")"
echo "  ✓ 四个读方法 → 一屏 JSON"

# ---- 说一句：turn/start → SSE ----------------------------------------------
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

# ---- 说过之后，面板里有东西了 ------------------------------------------------
# 这才是「驾驶舱」和「看日志」的区别：刚才那句话得变成首屏上看得见的东西。
curl -sS "$BASE/api/data" >"$WORK/data2.json"
grep -q '"sessions":\[\]' "$WORK/data2.json" && fail "说完一句，会话列表还是空的"
echo "  ✓ 说过的那一句出现在首屏里"

# ---- 说过的话，读得回来 ------------------------------------------------------
# 这才是 `/api/session` 存在的理由：刷新之后那场对话还在，切走再切回来也还在。
# 协议给的是**最新的在最前**（方便一页页往更早走），而且这儿只有两条，
# 所以 `nextCursor` 该是 null —— 「到底了」。
curl -sS "$BASE/api/session?sessionId=default" >"$WORK/session.json"
grep -q '"role":"user"'      "$WORK/session.json" || fail "历史里没有自己那一句"
grep -q '"role":"assistant"' "$WORK/session.json" || fail "历史里没有助手那一条"
grep -q '你好，世界'          "$WORK/session.json" || fail "历史里的答复不对"
grep -q '"nextCursor":null'  "$WORK/session.json" || fail "只有两条，不该有更早的游标"
echo "  历史：$(head -c 220 "$WORK/session.json")"
echo "  ✓ 说过的话读得回来（最新在前 + 已经到底）"

# 会话是硬边界：翻一个没人说过的会话，得是一页空的，而不是把 default 的漏过去。
curl -sS "$BASE/api/session?sessionId=nobody" | grep -q '"data":\[\]' \
  || fail "空会话不该看见别人的话"
echo "  ✓ 会话之间不串"

echo "✓ 驾驶舱冒烟测试通过"
