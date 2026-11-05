#!/usr/bin/env bash
# End-to-end smoke test for the long-task engine's crash recovery: a process that died
# mid-turn really gets continued by a fresh one.
#
# The chain this proves — and no unit test can: boot → migrate → a checkpoint left `open`
# by a kill -9 → **fresh process startup reconcile** → resume notifications on real stdout →
# the reply lands in the chat log under the original turn, checkpoint closes.
#
#   ./scripts/smoke-resume.sh
#
# Requires: a cargo build, python3 (fake SSE provider + state.db assertions).

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/joy-rs/target/debug/joy"
WORK="$(mktemp -d)"
FAKE_PID=""
APP_PID=""
cleanup() {
  [[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null || true
  [[ -n "$FAKE_PID" ]] && kill "$FAKE_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "✗ $1" >&2; exit 1; }

[[ -x "$BIN" ]] || fail "找不到 $BIN，先跑：cd joy-rs && cargo build"

# Fake SSE endpoint: one streaming reply ("你好，世界") is all the resumed turn needs —
# a resume skips the retrieval gate, so exactly one model call happens.
python3 "$REPO/scripts/fake_sse_server.py" >"$WORK/fake_port" &
FAKE_PID=$!
for _ in $(seq 1 50); do
  [[ -s "$WORK/fake_port" ]] && break
  sleep 0.1
done
MODEL_PORT="$(head -n1 "$WORK/fake_port" 2>/dev/null || true)"
[[ -n "$MODEL_PORT" ]] || fail "假 SSE 端点没起来"

mkdir -p "$WORK/.joy"
cat > "$WORK/.joy/models.json" <<EOF
{"active":"smoke","profiles":[{"id":"smoke","provider":"anthropic","base_url":"http://127.0.0.1:$MODEL_PORT","key_source":"env:JOY_API_KEY"}]}
EOF

# ---- Boot once: migrations run, then exit cleanly --------------------------------
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"session/list","params":{}}' \
  | JOY_HOME="$WORK/.joy" JOY_PROVIDER=anthropic JOY_API_KEY=test \
      JOY_BASE_URL="http://127.0.0.1:$MODEL_PORT" \
      "$BIN" app-server >"$WORK/out1.log" 2>&1
grep -q '"id":1' "$WORK/out1.log" || { cat "$WORK/out1.log"; fail "第一次启动没应答"; }
echo "  ✓ 启动跑完迁移（0013 turn_checkpoints 在库里）"

# ---- The crash leftover: a checkpoint the killed process never got to close --------
# The working memory is exactly what the checkpoint outlet serializes: user asked,
# assistant requested a tool, the result came back — then the process died.
python3 - "$WORK/.joy/state.db" <<'EOF' || fail "预置 checkpoint 失败"
import json, sqlite3, sys
con = sqlite3.connect(sys.argv[1])
messages = [
    {"role": "user", "content": [{"type": "text", "text": "帮我盯三天服务器"}]},
    {"role": "assistant", "content": [
        {"type": "tool_use", "id": "tu_1", "name": "save_note",
         "input": {"subject": "服务器", "content": "要盯三天"}}]},
    {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "tu_1", "content": "已记录"}]},
]
con.execute(
    "INSERT INTO turn_checkpoints"
    " (turn_id, session_id, user_message, messages_json, iterations, usage_json)"
    " VALUES (?, ?, ?, ?, ?, ?)",
    ("t-crash", "default", "帮我盯三天服务器",
     json.dumps(messages, ensure_ascii=False), 2,
     json.dumps({"input_tokens": 100, "output_tokens": 20})),
)
con.commit()
status = con.execute(
    "SELECT status FROM turn_checkpoints WHERE turn_id = 't-crash'"
).fetchone()[0]
assert status == "open", status
EOF
echo "  ✓ 崩溃残留：t-crash 挂在 open（已完成 2 轮）"

# ---- Fresh process: startup reconcile resumes the turn ----------------------------
mkfifo "$WORK/in"
JOY_HOME="$WORK/.joy" JOY_PROVIDER=anthropic JOY_API_KEY=test \
  JOY_BASE_URL="http://127.0.0.1:$MODEL_PORT" \
  "$BIN" app-server <"$WORK/in" >"$WORK/out2.log" 2>&1 &
APP_PID=$!
exec 3>"$WORK/in" # hold stdin open: the reconcile task needs the process alive

RESUMED=""
for _ in $(seq 1 100); do
  if grep -q '"type":"turnCompleted"' "$WORK/out2.log" 2>/dev/null; then
    RESUMED=yes
    break
  fi
  kill -0 "$APP_PID" 2>/dev/null || break
  sleep 0.1
done
[[ -n "$RESUMED" ]] || { cat "$WORK/out2.log"; fail "重启后没等到 turnCompleted —— 对账没续跑"; }
exec 3>&-
wait "$APP_PID" 2>/dev/null || true
APP_PID=""

grep -q '"type":"turnResumed"' "$WORK/out2.log" || fail "没有 turnResumed 通知"
grep -q '"fromIterations":2' "$WORK/out2.log" || fail "续跑没报出已完成轮数"
grep -q '"reply":"你好，世界"' "$WORK/out2.log" || fail "续跑的回复不对"
grep -q '"type":"turnStarted"' "$WORK/out2.log" || fail "续跑该以 turnStarted 开场"
echo "  stdout："
sed 's/^/    /' "$WORK/out2.log" | head -n 8
echo "  ✓ 重启对账：turnResumed（from 2 轮）→ turnCompleted"

# ---- The ledger closes the loop: checkpoint done, reply in the chat log ------------
python3 - "$WORK/.joy/state.db" <<'EOF' || fail "收尾状态不对"
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
(status, iters) = con.execute(
    "SELECT status, iterations FROM turn_checkpoints WHERE turn_id = 't-crash'"
).fetchone()
assert status == "done", f"checkpoint 该是 done：{status}"
# A pure-text resume completes without crossing another tool-results boundary, so the
# count stays at the crashed process's last checkpoint (2) — no phantom progress.
assert iters == 2, f"迭代数该停在崩溃前的最后一个边界：{iters}"
reply = con.execute(
    "SELECT content FROM chat_log WHERE role = 'assistant' ORDER BY id DESC LIMIT 1"
).fetchone()[0]
assert "你好，世界" in reply, reply
EOF
echo "  ✓ 台账闭环：checkpoint=done（停在崩溃前的边界），回复落在对话日志里"

echo "✓ 续跑冒烟测试通过"
