#!/usr/bin/env bash
# End-to-end smoke for M5's business form: the task API, inbound webhooks, and the tier pin.
#
# What it proves, with a real binary and a fake provider (no keys):
#   1. /api/run is one turn in, one JSON answer out (no SSE) — programs can call it;
#   2. a declared tier **beats fixed mode**: the answering model is the tier's profile, and
#      `routeReason` says why (visible, not a guess);
#   3. the routing table reads back and saves through /api/model/routes{,/save};
#   4. /api/hook/<name>: no config → 404, wrong secret → 401, right secret → one turn;
#   5. a task's declared tier applies to its turn even when the caller does not repeat it;
#   6. phase 2 fences: a stale signature → 401, a replayed one → 409, over the per-minute cap
#      → 429 + Retry-After, and an async hook answers 202 while its answer goes to the callback.
#
#   ./scripts/smoke-business.sh
#
# Requires: a cargo build, a built frontend (just dashboard-build), python3.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/joy-rs/target/debug/joy"
ASSETS="$REPO/joy-ts/packages/dashboard/dist"
WORK="$(mktemp -d)"
FAKE_PID=""
DASH_PID=""
CB_PID=""
cleanup() {
  [[ -n "$DASH_PID" ]] && kill "$DASH_PID" 2>/dev/null || true
  [[ -n "$FAKE_PID" ]] && kill "$FAKE_PID" 2>/dev/null || true
  [[ -n "$CB_PID" ]] && kill "$CB_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "✗ $1" >&2; exit 1; }

[[ -x "$BIN" ]] || fail "找不到 $BIN，先跑：cd joy-rs && cargo build"
[[ -f "$ASSETS/index.html" ]] || fail "前端还没构建，先跑：just dashboard-build"

# Six streamed turns: /api/run, three webhook modes (plain / signed / async), the capped hook's
# one allowed knock, and the task's turn.
python3 "$REPO/scripts/fake_sse_server.py" 6 >"$WORK/fake_port" &
FAKE_PID=$!
for _ in $(seq 1 50); do
  [[ -s "$WORK/fake_port" ]] && break
  sleep 0.1
done
MODEL_PORT="$(head -n1 "$WORK/fake_port" 2>/dev/null || true)"
[[ -n "$MODEL_PORT" ]] || fail "假 SSE 端点没起来"

# The async hook's callback receiver: it records what Joy pushed, one JSON per line.
python3 "$REPO/scripts/fake_callback_server.py" "$WORK/callback.jsonl" 1 >"$WORK/cb_port" &
CB_PID=$!
for _ in $(seq 1 50); do
  [[ -s "$WORK/cb_port" ]] && break
  sleep 0.1
done
CB_PORT="$(head -n1 "$WORK/cb_port" 2>/dev/null || true)"
[[ -n "$CB_PORT" ]] || fail "假回调接收方没起来"

PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
mkdir -p "$WORK/.joy"

# Two profiles: "smoke" (active, chat) and "deep" (reasoning). Different model names are the
# only way to see *which* one answered — the fake endpoint does not care who it is.
cat >"$WORK/.joy/models.json" <<EOF
{"active":"smoke","profiles":[
  {"id":"smoke","provider":"anthropic","model":"chat-model","base_url":"http://127.0.0.1:$MODEL_PORT","key_source":"env:JOY_API_KEY"},
  {"id":"deep","provider":"anthropic","model":"deep-model","base_url":"http://127.0.0.1:$MODEL_PORT","key_source":"env:JOY_API_KEY"}
]}
EOF

# fixed mode with a reasoning row: the default policy, and a pin that must still win over it.
cat >"$WORK/.joy/model-routes.json" <<'EOF_ROUTES'
{"mode":"fixed","routes":{"chat":"smoke","reasoning":"deep"}}
EOF_ROUTES

JOY_PORT="$PORT" \
JOY_HOME="$WORK/.joy" \
JOY_HOOK_SECRET=hook-secret \
JOY_PROVIDER=anthropic JOY_API_KEY=test JOY_BASE_URL="http://127.0.0.1:$MODEL_PORT" \
  "$BIN" dashboard >"$WORK/dash.log" 2>&1 &
DASH_PID=$!

BASE=""
for _ in $(seq 1 100); do
  BASE="$(grep -oE '(localhost|127\.0\.0\.1):[0-9]+' "$WORK/dash.log" | head -n1 || true)"
  [[ -n "$BASE" ]] && break
  kill -0 "$DASH_PID" 2>/dev/null || { cat "$WORK/dash.log"; fail "驾驶舱没起来就退了"; }
  sleep 0.1
done
[[ -n "$BASE" ]] || { cat "$WORK/dash.log"; fail "等不到启动行"; }
BASE="http://$BASE"

# ---- The routing table -------------------------------------------------------------
curl -sS "$BASE/api/model/routes" >"$WORK/routes1.json"
grep -q '"mode":"fixed"' "$WORK/routes1.json" || fail "默认该是 fixed：$(cat "$WORK/routes1.json")"
grep -q '"tier":"reasoning","profileId":"deep"' "$WORK/routes1.json" \
  || fail "reasoning 那一行不对：$(cat "$WORK/routes1.json")"
grep -q '"tier":"fast","profileId":null' "$WORK/routes1.json" \
  || fail "没配的档位该是 null（跟随默认）"
echo "  ✓ 路由表：三行齐全，没配的就是 null"

curl -sS -X POST "$BASE/api/model/routes/save" -H 'content-type: application/json' \
  -d '{"mode":"auto","routes":[
        {"tier":"fast","profileId":"smoke"},
        {"tier":"chat","profileId":"smoke"},
        {"tier":"reasoning","profileId":"deep"}]}' >"$WORK/routes2.json"
grep -q '"mode":"auto"' "$WORK/routes2.json" || fail "保存后该是 auto：$(cat "$WORK/routes2.json")"
curl -sS "$BASE/api/model/routes" | grep -q '"mode":"auto"' || fail "auto 没落盘"
echo "  ✓ 路由表：fixed → auto 已落盘"

# A typo is refused before it can cost money.
curl -sS -X POST "$BASE/api/model/routes/save" -H 'content-type: application/json' \
  -d '{"mode":"auto","routes":[{"tier":"reasoning","profileId":"nope"}]}' >"$WORK/routes3.json"
grep -q '不存在' "$WORK/routes3.json" || fail "不存在的 profile 该被拒：$(cat "$WORK/routes3.json")"
echo "  ✓ 路由表：指向不存在的 profile 会被拒"

# ---- The task API: one turn in, one JSON answer out ---------------------------------
curl -sS -X POST "$BASE/api/run" -H 'content-type: application/json' \
  -d '{"message":"你好","tier":"reasoning"}' >"$WORK/run1.json" || fail "POST /api/run 没通"
sed 's/^/    /' "$WORK/run1.json"
grep -q '"reply":"你好，世界"' "$WORK/run1.json" || fail "一次性应答里没有完整答复"
grep -q '"model":"deep-model"' "$WORK/run1.json" \
  || fail "tier:reasoning 必须走 deep 那套（这就是档位的作用）"
grep -q '"routeReason":"tier:reasoning → deep"' "$WORK/run1.json" \
  || fail "routeReason 没说清为什么选它"
echo "  ✓ 任务 API：一轮进、一段 JSON 出，档位压过 fixed 模式"

# A typo in the tier is an error, not a silent fallback.
curl -sS -X POST "$BASE/api/run" -H 'content-type: application/json' \
  -d '{"message":"你好","tier":"reasoining"}' >"$WORK/run2.json"
grep -q 'tier 只能是' "$WORK/run2.json" || fail "拼错的档位该被拒：$(cat "$WORK/run2.json")"
echo "  ✓ 任务 API：拼错的档位当场拒绝（不猜）"

# ---- Inbound webhook ---------------------------------------------------------------
BODY='{"status":"failed","service":"api"}'
HOOK_SECRET=hook-secret

curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/hook/deploy" \
  -H 'content-type: application/json' -d "$BODY" | grep -q '^404$' \
  || fail "没配 webhooks.json 的时候，这扇门不该存在"

cat >"$WORK/.joy/webhooks.json" <<EOF_HOOKS
{"hooks": [
  {
    "name": "deploy",
    "secret": "env:JOY_HOOK_SECRET",
    "prompt": "线上出事了，按这个事件处理：\n{{payload}}",
    "tier": "reasoning"
  },
  {
    "name": "signed",
    "secret": "env:JOY_HOOK_SECRET",
    "prompt": "看 {{payload}}",
    "signature": true
  },
  {
    "name": "capped",
    "secret": "env:JOY_HOOK_SECRET",
    "prompt": "看 {{payload}}",
    "limit": 1
  },
  {
    "name": "async",
    "secret": "env:JOY_HOOK_SECRET",
    "prompt": "看 {{payload}}",
    "callback": "http://127.0.0.1:$CB_PORT/result"
  }
]}
EOF_HOOKS

# Same HMAC the other side computes with `openssl dgst -sha256 -hmac` over `<timestamp>.<body>`.
sign() {
  python3 -c 'import hashlib, hmac, sys; print(hmac.new(sys.argv[1].encode(), (sys.argv[2] + "." + sys.argv[3]).encode(), hashlib.sha256).hexdigest())' "$1" "$2" "$3"
}

curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/hook/deploy" \
  -H 'content-type: application/json' -H 'x-joy-secret: wrong' -d "$BODY" | grep -q '^401$' \
  || fail "密钥不对必须 401"

curl -sS -X POST "$BASE/api/hook/deploy" -H 'content-type: application/json' \
  -H 'x-joy-secret: hook-secret' -d "$BODY" >"$WORK/hook.json" || fail "正确的密钥应该放行"
grep -q '"reply":"你好，世界"' "$WORK/hook.json" || fail "webhook 没跑出一轮答复：$(cat "$WORK/hook.json")"
grep -q '"model":"deep-model"' "$WORK/hook.json" || fail "hook 声明的档位没生效"
echo "  ✓ 入站 webhook：无配置 404 / 错密钥 401 / 对密钥跑一轮，档位跟着 hook 走"

# ---- Phase 2: freshness, replay, rate, async ---------------------------------------
NOW="$(date +%s)"
STALE="$((NOW - 600))"
SIG="$(sign "$HOOK_SECRET" "$NOW" "$BODY")"
SIG_STALE="$(sign "$HOOK_SECRET" "$STALE" "$BODY")"

curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/hook/signed" \
  -H 'content-type: application/json' -H "x-joy-timestamp: $STALE" \
  -H "x-joy-signature: $SIG_STALE" -d "$BODY" | grep -q '^401$' \
  || fail "过期的签名必须 401"

curl -sS -X POST "$BASE/api/hook/signed" -H 'content-type: application/json' \
  -H "x-joy-timestamp: $NOW" -H "x-joy-signature: $SIG" -d "$BODY" >"$WORK/signed.json" \
  || fail "签名对的请求没通"
grep -q '"reply":"你好，世界"' "$WORK/signed.json" || fail "签名模式没跑出一轮：$(cat "$WORK/signed.json")"

curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/hook/signed" \
  -H 'content-type: application/json' -H "x-joy-timestamp: $NOW" \
  -H "x-joy-signature: $SIG" -d "$BODY" | grep -q '^409$' \
  || fail "同一个签名第二次必须是 409（重放）"
echo "  ✓ 重放防护：签名带时间戳，过期 401 / 重放 409"

curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/hook/capped" \
  -H 'content-type: application/json' -H 'x-joy-secret: hook-secret' -d '{}' | grep -q '^200$' \
  || fail "限流 hook 的第一次该放行"
HEAD="$(curl -sS -D - -o /dev/null -X POST "$BASE/api/hook/capped" \
  -H 'content-type: application/json' -H 'x-joy-secret: hook-secret' -d '{}')"
grep -qi '^HTTP/1.1 429' <<<"$HEAD" || fail "超过每分钟额度必须 429"
grep -qiE '^retry-after: [0-9]+' <<<"$HEAD" || fail "429 要带 Retry-After：$HEAD"
echo "  ✓ 限流：每分钟 1 次，超了 429 + Retry-After"

curl -sS -o "$WORK/accepted.json" -w '%{http_code}' -X POST "$BASE/api/hook/async" \
  -H 'content-type: application/json' -H 'x-joy-secret: hook-secret' -d 'boom' \
  | grep -q '^202$' || fail "异步 hook 必须立刻 202"
grep -q '"accepted":true' "$WORK/accepted.json" || fail "202 的 body 要说清收下了：$(cat "$WORK/accepted.json")"

for _ in $(seq 1 100); do
  [[ -s "$WORK/callback.jsonl" ]] && break
  sleep 0.1
done
[[ -s "$WORK/callback.jsonl" ]] || fail "回调一直没送到"
grep -q '"ok":true' "$WORK/callback.jsonl" || fail "回调内容不对：$(cat "$WORK/callback.jsonl")"
grep -q '"hook":"async"' "$WORK/callback.jsonl" || fail "回调里没说是哪个 hook"
grep -q '你好，世界' "$WORK/callback.jsonl" || fail "回调里没有答复"
echo "  ✓ 异步回调：202 立刻收下，答复推到 callback"

# ---- A task's tier applies to its own turn -----------------------------------------
curl -sS -X POST "$BASE/api/task" -H 'content-type: application/json' \
  -d '{"action":"create","sessionId":"biz","title":"深读一遍","tier":"reasoning"}' >"$WORK/task.json"
grep -q '"tier":"reasoning"' "$WORK/task.json" || fail "任务的档位没落库：$(cat "$WORK/task.json")"

# The caller does not repeat the tier: the task carries it into the turn.
curl -sS -X POST "$BASE/api/run" -H 'content-type: application/json' \
  -d '{"sessionId":"biz","message":"开始"}' >"$WORK/run3.json"
grep -q '"model":"deep-model"' "$WORK/run3.json" \
  || fail "任务的档位没带进这一轮：$(cat "$WORK/run3.json")"
echo "  ✓ 任务：档位挂在任务上，这一轮不用再说一次"

echo "✓ 业务形态冒烟测试通过（任务 API / webhook / 档位路由）"
