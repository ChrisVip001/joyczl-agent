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

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ASSETS="$REPO/joy-ts/packages/dashboard/dist"

fail() { echo "✗ $1" >&2; exit 1; }

need_bin
[[ -f "$ASSETS/index.html" ]] || fail "the front end has not been built; run: just dashboard-build"

# Six streamed turns: /api/run, three webhook modes (plain / signed / async), the capped hook's
# one allowed knock, and the task's turn.
python3 "$REPO/scripts/fake_sse_server.py" 6 >"$WORK/fake_port" &
FAKE_PID=$!
track_pid "$FAKE_PID"
for _ in $(seq 1 50); do
  [[ -s "$WORK/fake_port" ]] && break
  sleep 0.1
done
MODEL_PORT="$(head -n1 "$WORK/fake_port" 2>/dev/null || true)"
[[ -n "$MODEL_PORT" ]] || fail "the fake SSE endpoint did not come up"

# The async hook's callback receiver: it records what Joy pushed, one JSON per line.
python3 "$REPO/scripts/fake_callback_server.py" "$WORK/callback.jsonl" 1 >"$WORK/cb_port" &
CB_PID=$!
track_pid "$CB_PID"
for _ in $(seq 1 50); do
  [[ -s "$WORK/cb_port" ]] && break
  sleep 0.1
done
CB_PORT="$(head -n1 "$WORK/cb_port" 2>/dev/null || true)"
[[ -n "$CB_PORT" ]] || fail "the fake callback receiver did not come up"

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
track_pid "$DASH_PID"

BASE=""
for _ in $(seq 1 100); do
  BASE="$(grep -oE '(localhost|127\.0\.0\.1):[0-9]+' "$WORK/dash.log" | head -n1 || true)"
  [[ -n "$BASE" ]] && break
  kill -0 "$DASH_PID" 2>/dev/null || { cat "$WORK/dash.log"; fail "the cockpit exited before it came up"; }
  sleep 0.1
done
[[ -n "$BASE" ]] || { cat "$WORK/dash.log"; fail "no startup line ever appeared"; }
BASE="http://$BASE"

# ---- The routing table -------------------------------------------------------------
curl -sS "$BASE/api/model/routes" >"$WORK/routes1.json"
grep -q '"mode":"fixed"' "$WORK/routes1.json" || fail "the default should be fixed: $(cat "$WORK/routes1.json")"
grep -q '"tier":"reasoning","profileId":"deep"' "$WORK/routes1.json" \
  || fail "the reasoning row is wrong: $(cat "$WORK/routes1.json")"
grep -q '"tier":"fast","profileId":null' "$WORK/routes1.json" \
  || fail "an unconfigured tier should be null (follow the default)"
echo "  ✓ routing table: all three rows, the unset one is null"

curl -sS -X POST "$BASE/api/model/routes/save" -H 'content-type: application/json' \
  -d '{"mode":"auto","routes":[
        {"tier":"fast","profileId":"smoke"},
        {"tier":"chat","profileId":"smoke"},
        {"tier":"reasoning","profileId":"deep"}]}' >"$WORK/routes2.json"
grep -q '"mode":"auto"' "$WORK/routes2.json" || fail "after saving it should be auto: $(cat "$WORK/routes2.json")"
curl -sS "$BASE/api/model/routes" | grep -q '"mode":"auto"' || fail "auto never reached the disk"
echo "  ✓ routing table: fixed → auto, written to disk"

# A typo is refused before it can cost money.
curl -sS -X POST "$BASE/api/model/routes/save" -H 'content-type: application/json' \
  -d '{"mode":"auto","routes":[{"tier":"reasoning","profileId":"nope"}]}' >"$WORK/routes3.json"
grep -q 'does not exist' "$WORK/routes3.json" || fail "a profile that does not exist should be refused: $(cat "$WORK/routes3.json")"
echo "  ✓ routing table: pointing at a profile that does not exist is refused"

# ---- The task API: one turn in, one JSON answer out ---------------------------------
curl -sS -X POST "$BASE/api/run" -H 'content-type: application/json' \
  -d '{"message":"hello","tier":"reasoning"}' >"$WORK/run1.json" || fail "POST /api/run did not go through"
sed 's/^/    /' "$WORK/run1.json"
grep -q '"reply":"hello, world"' "$WORK/run1.json" || fail "the one-shot reply carries no full answer"
grep -q '"model":"deep-model"' "$WORK/run1.json" \
  || fail "tier:reasoning has to take the deep profile (that is what a tier is for)"
grep -q '"routeReason":"tier:reasoning → deep"' "$WORK/run1.json" \
  || fail "routeReason does not say why it picked that one"
echo "  ✓ task API: one turn in, one JSON answer out; the tier beats fixed mode"

# A typo in the tier is an error, not a silent fallback.
curl -sS -X POST "$BASE/api/run" -H 'content-type: application/json' \
  -d '{"message":"hello","tier":"reasoining"}' >"$WORK/run2.json"
grep -q 'tier must be one of' "$WORK/run2.json" || fail "a misspelt tier should be refused: $(cat "$WORK/run2.json")"
echo "  ✓ task API: a misspelt tier is refused on the spot (no guessing)"

# ---- Inbound webhook ---------------------------------------------------------------
BODY='{"status":"failed","service":"api"}'
HOOK_SECRET=hook-secret

curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/hook/deploy" \
  -H 'content-type: application/json' -d "$BODY" | grep -q '^404$' \
  || fail "with no webhooks.json this door should not exist"

cat >"$WORK/.joy/webhooks.json" <<EOF_HOOKS
{"hooks": [
  {
    "name": "deploy",
    "secret": "env:JOY_HOOK_SECRET",
    "prompt": "something happened in production; handle this event:\n{{payload}}",
    "tier": "reasoning"
  },
  {
    "name": "signed",
    "secret": "env:JOY_HOOK_SECRET",
    "prompt": "look at {{payload}}",
    "signature": true
  },
  {
    "name": "capped",
    "secret": "env:JOY_HOOK_SECRET",
    "prompt": "look at {{payload}}",
    "limit": 1
  },
  {
    "name": "async",
    "secret": "env:JOY_HOOK_SECRET",
    "prompt": "look at {{payload}}",
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
  || fail "a wrong secret must be 401"

curl -sS -X POST "$BASE/api/hook/deploy" -H 'content-type: application/json' \
  -H 'x-joy-secret: hook-secret' -d "$BODY" >"$WORK/hook.json" || fail "the right secret should be let through"
grep -q '"reply":"hello, world"' "$WORK/hook.json" || fail "the webhook ran no turn: $(cat "$WORK/hook.json")"
grep -q '"model":"deep-model"' "$WORK/hook.json" || fail "the tier the hook declared did not take effect"
echo "  ✓ inbound webhook: no config 404 / wrong secret 401 / right secret runs one turn, the tier follows the hook"

# ---- Phase 2: freshness, replay, rate, async ---------------------------------------
NOW="$(date +%s)"
STALE="$((NOW - 600))"
SIG="$(sign "$HOOK_SECRET" "$NOW" "$BODY")"
SIG_STALE="$(sign "$HOOK_SECRET" "$STALE" "$BODY")"

curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/hook/signed" \
  -H 'content-type: application/json' -H "x-joy-timestamp: $STALE" \
  -H "x-joy-signature: $SIG_STALE" -d "$BODY" | grep -q '^401$' \
  || fail "a stale signature must be 401"

curl -sS -X POST "$BASE/api/hook/signed" -H 'content-type: application/json' \
  -H "x-joy-timestamp: $NOW" -H "x-joy-signature: $SIG" -d "$BODY" >"$WORK/signed.json" \
  || fail "the correctly signed request did not go through"
grep -q '"reply":"hello, world"' "$WORK/signed.json" || fail "signed mode ran no turn: $(cat "$WORK/signed.json")"

curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/hook/signed" \
  -H 'content-type: application/json' -H "x-joy-timestamp: $NOW" \
  -H "x-joy-signature: $SIG" -d "$BODY" | grep -q '^409$' \
  || fail "the same signature a second time must be 409 (replay)"
echo "  ✓ replay protection: the signature carries a timestamp; stale 401 / replayed 409"

curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/hook/capped" \
  -H 'content-type: application/json' -H 'x-joy-secret: hook-secret' -d '{}' | grep -q '^200$' \
  || fail "the first knock at the rate-limited hook should pass"
HEAD="$(curl -sS -D - -o /dev/null -X POST "$BASE/api/hook/capped" \
  -H 'content-type: application/json' -H 'x-joy-secret: hook-secret' -d '{}')"
grep -qi '^HTTP/1.1 429' <<<"$HEAD" || fail "over the per-minute allowance must be 429"
grep -qiE '^retry-after: [0-9]+' <<<"$HEAD" || fail "a 429 has to carry Retry-After: $HEAD"
echo "  ✓ rate limit: once a minute; over that, 429 + Retry-After"

curl -sS -o "$WORK/accepted.json" -w '%{http_code}' -X POST "$BASE/api/hook/async" \
  -H 'content-type: application/json' -H 'x-joy-secret: hook-secret' -d 'boom' \
  | grep -q '^202$' || fail "an async hook must answer 202 at once"
grep -q '"accepted":true' "$WORK/accepted.json" || fail "the 202 body has to say it took the work: $(cat "$WORK/accepted.json")"

for _ in $(seq 1 100); do
  [[ -s "$WORK/callback.jsonl" ]] && break
  sleep 0.1
done
[[ -s "$WORK/callback.jsonl" ]] || fail "the callback never arrived"
grep -q '"ok":true' "$WORK/callback.jsonl" || fail "the callback content is wrong: $(cat "$WORK/callback.jsonl")"
grep -q '"hook":"async"' "$WORK/callback.jsonl" || fail "the callback does not say which hook"
grep -q 'hello, world' "$WORK/callback.jsonl" || fail "the callback carries no answer"
echo "  ✓ async callback: 202 taken at once, the answer pushed to the callback"

# ---- A task's tier applies to its own turn -----------------------------------------
curl -sS -X POST "$BASE/api/task" -H 'content-type: application/json' \
  -d '{"action":"create","sessionId":"biz","title":"a deep read","tier":"reasoning"}' >"$WORK/task.json"
grep -q '"tier":"reasoning"' "$WORK/task.json" || fail "the task's tier never reached the store: $(cat "$WORK/task.json")"

# The caller does not repeat the tier: the task carries it into the turn.
curl -sS -X POST "$BASE/api/run" -H 'content-type: application/json' \
  -d '{"sessionId":"biz","message":"start"}' >"$WORK/run3.json"
grep -q '"model":"deep-model"' "$WORK/run3.json" \
  || fail "the task's tier did not reach the turn: $(cat "$WORK/run3.json")"
echo "  ✓ task: the tier hangs off the task, so the turn need not repeat it"

echo "✓ business smoke test passed (task API / webhook / tier routing)"
