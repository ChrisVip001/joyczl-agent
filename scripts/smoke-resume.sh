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

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

fail() { echo "✗ $1" >&2; exit 1; }

need_bin

# Fake SSE endpoint: one streaming reply ("hello, world") is all the resumed turn needs —
# a resume skips the retrieval gate, so exactly one model call happens.
python3 "$REPO/scripts/fake_sse_server.py" >"$WORK/fake_port" &
FAKE_PID=$!
track_pid "$FAKE_PID"
for _ in $(seq 1 50); do
  [[ -s "$WORK/fake_port" ]] && break
  sleep 0.1
done
MODEL_PORT="$(head -n1 "$WORK/fake_port" 2>/dev/null || true)"
[[ -n "$MODEL_PORT" ]] || fail "the fake SSE endpoint did not come up"

mkdir -p "$WORK/.joy"
cat > "$WORK/.joy/models.json" <<EOF
{"active":"smoke","profiles":[{"id":"smoke","provider":"anthropic","base_url":"http://127.0.0.1:$MODEL_PORT","key_source":"env:JOY_API_KEY"}]}
EOF

# ---- Boot once: migrations run, then exit cleanly --------------------------------
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"session/list","params":{}}' \
  | JOY_HOME="$WORK/.joy" JOY_PROVIDER=anthropic JOY_API_KEY=test \
      JOY_BASE_URL="http://127.0.0.1:$MODEL_PORT" \
      "$BIN" app-server >"$WORK/out1.log" 2>&1
grep -q '"id":1' "$WORK/out1.log" || { cat "$WORK/out1.log"; fail "the first boot did not answer"; }
echo "  ✓ boot ran the migrations (0013 turn_checkpoints is in the store)"

# ---- The crash leftover: a checkpoint the killed process never got to close --------
# The working memory is exactly what the checkpoint outlet serializes: user asked,
# assistant requested a tool, the result came back — then the process died.
python3 - "$WORK/.joy/state.db" <<'EOF' || fail "seeding the checkpoint failed"
import json, sqlite3, sys
con = sqlite3.connect(sys.argv[1])
messages = [
    {"role": "user", "content": [{"type": "text", "text": "watch the server for me for three days"}]},
    {"role": "assistant", "content": [
        {"type": "tool_use", "id": "tu_1", "name": "save_note",
         "input": {"subject": "server", "content": "watch it for three days"}}]},
    {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "tu_1", "content": "noted"}]},
]
con.execute(
    "INSERT INTO turn_checkpoints"
    " (turn_id, session_id, user_message, messages_json, iterations, usage_json)"
    " VALUES (?, ?, ?, ?, ?, ?)",
    ("t-crash", "default", "watch the server for me for three days",
     json.dumps(messages, ensure_ascii=False), 2,
     json.dumps({"input_tokens": 100, "output_tokens": 20})),
)
# 0.15: the same crash leaves a task mid-flight, an open run and one step that was **sent but
# never landed**. The step must come back as `waiting` — a write that may already have happened
# is never replayed.
con.execute(
    "INSERT INTO agent_tasks (id, session_id, title, acceptance_json, status, current_turn_id)"
    " VALUES ('task-smoke', 'default', 'write a file', '[]', 'running', 't-crash')"
)
con.execute(
    "INSERT INTO task_runs (id, task_id, session_id, turn_id, status)"
    " VALUES ('t-crash', 'task-smoke', 'default', 't-crash', 'running')"
)
con.execute(
    "INSERT INTO task_steps"
    " (task_id, run_id, seq, kind, tool, input_json, status, idempotency_key)"
    " VALUES ('task-smoke', 't-crash', 1, 'tool', 'write_file',"
    " '{\"path\":\"/tmp/half-written.md\"}', 'running', 't-crash:1')"
)
con.commit()
status = con.execute(
    "SELECT status FROM turn_checkpoints WHERE turn_id = 't-crash'"
).fetchone()[0]
assert status == "open", status
EOF
echo "  ✓ crash leftovers: t-crash sits at open (2 rounds done) plus one run step «sent but never landed»"

# ---- Fresh process: startup reconcile resumes the turn ----------------------------
mkfifo "$WORK/in"
JOY_HOME="$WORK/.joy" JOY_PROVIDER=anthropic JOY_API_KEY=test \
  JOY_BASE_URL="http://127.0.0.1:$MODEL_PORT" \
  "$BIN" app-server <"$WORK/in" >"$WORK/out2.log" 2>&1 &
APP_PID=$!
track_pid "$APP_PID"
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
[[ -n "$RESUMED" ]] || { cat "$WORK/out2.log"; fail "no turnCompleted after the restart — the reconcile did not resume it"; }
exec 3>&-
wait "$APP_PID" 2>/dev/null || true

grep -q '"type":"turnResumed"' "$WORK/out2.log" || fail "no turnResumed notification"
grep -q '"fromIterations":2' "$WORK/out2.log" || fail "the resume did not report how many rounds were done"
grep -q '"reply":"hello, world"' "$WORK/out2.log" || fail "the resumed reply is wrong"
grep -q '"type":"turnStarted"' "$WORK/out2.log" || fail "a resume should open with turnStarted"
echo "  stdout："
sed 's/^/    /' "$WORK/out2.log" | head -n 8
echo "  ✓ startup reconcile: turnResumed (from 2 rounds) → turnCompleted"

# ---- The ledger closes the loop: checkpoint done, reply in the chat log ------------
python3 - "$WORK/.joy/state.db" <<'EOF' || fail "the closing status is wrong"
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
(status, iters) = con.execute(
    "SELECT status, iterations FROM turn_checkpoints WHERE turn_id = 't-crash'"
).fetchone()
assert status == "done", f"the checkpoint should be done: {status}"
# A pure-text resume completes without crossing another tool-results boundary, so the
# count stays at the crashed process's last checkpoint (2) — no phantom progress.
assert iters == 2, f"the iteration count should stop at the last boundary before the crash: {iters}"
reply = con.execute(
    "SELECT content FROM chat_log WHERE role = 'assistant' ORDER BY id DESC LIMIT 1"
).fetchone()[0]
assert "hello, world" in reply, reply

# ---- 0.15's task ledger: a side effect cut in half is stopped, never replayed ---------------
(step_status, step_note) = con.execute(
    "SELECT status, note FROM task_steps WHERE idempotency_key = 't-crash:1'"
).fetchone()
assert step_status == "waiting", f"a step sent but never landed should turn waiting (not be replayed): {step_status}"
assert "no replay" in (step_note or "") or step_note, f"it has to say why it stopped: {step_note}"
run_status = con.execute(
    "SELECT status FROM task_runs WHERE id = 't-crash'"
).fetchone()[0]
assert run_status in ("waiting", "completed"), f"the run record has to close: {run_status}"
# the task this resume took over: no acceptance conditions → done the moment the turn ends (a restart must not leave a task running forever)
task_status = con.execute(
    "SELECT status FROM agent_tasks WHERE id = 'task-smoke'"
).fetchone()[0]
assert task_status == "done", f"after the resume the task should be done: {task_status}"
EOF
echo "  ✓ the ledger closes: checkpoint=done (at the pre-crash boundary), the reply lands in the chat log"
echo "  ✓ task ledger: the unlanded step turns waiting (no replay), and the task is closed out to done by this resume"

echo "✓ resume smoke test passed"
