#!/usr/bin/env bash
# app-server 的端到端冒烟测试：真的起一个 joy app-server，从 stdin 喂
# JSON-RPC，检查每种应答。比单测更靠近真实用法 —— 单测证明 state 对，
# 这个证明「协议 + 传输 + state」这条链路通。
#
#   ./scripts/smoke.sh
#
# 需要 cargo build 过（它直接用 target/debug/joy）。

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

# 一个会话里连着问：写入 → 检索 → 忘掉 → 配置 → 未实现 → 未知方法 → 坏 JSON。
# 注意：这个环境里没有 API key，所以 turn/start 应当报 PROVIDER_ERROR，
# 且错误信息要告诉用户怎么办 —— 而其它方法必须照常工作。
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

# ---- 断言 ----
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

# 记忆得真的是一个能打开的文件 —— 这是「local-first」的全部意义。
[[ -f "$WORK/.joy/state.db" ]] || fail "state.db 没建在 .joy/ 下"
sqlite3 "$WORK/.joy/state.db" '.tables' >/dev/null 2>&1 \
  && echo "  ✓ state.db 能被 sqlite3 直接打开" \
  || echo "  ⚠ 没装 sqlite3，跳过直接打开检查"

# 起一个假 SSE 端点，把端口打出来。下面两段各要一个新端口（它回完一次
# 流式应答就自己退场了）。
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

# ---- 流式：接到本地假 SSE 端点，验证 SSE → textDelta → 完整应答这条链 -------
# 不用真 key：JOY_BASE_URL 指到假端点，provider 照常打请求，只是对端是假的。
PORT="$(start_fake "$WORK/fake_port")"
[[ -n "$PORT" ]] || fail "假 SSE 端点没起来"

STREAM_OUT=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"turn/start","params":{"message":"你好"}}' \
  | JOY_PROVIDER=anthropic JOY_API_KEY=test JOY_BASE_URL="http://127.0.0.1:$PORT" \
    JOY_HOME="$WORK/.joy-stream" "$BIN" app-server)

echo "$STREAM_OUT" | sed 's/^/  [stream] /'

echo "$STREAM_OUT" | grep -q '"method":"turn/notification"' || fail "流式没有通知发出"
echo "$STREAM_OUT" | grep -q '"textDelta"'                 || fail "没有 textDelta 通知"
echo "$STREAM_OUT" | grep -q '"delta":"你好"'              || fail "第一个增量不对"
echo "$STREAM_OUT" | grep -q '"reply":"你好，世界"'         || fail "最终应答不是拼起来的完整文本"
echo "$STREAM_OUT" | grep -q '"id":1,"result":{"turnId"'   || fail "turn/start 没有正常应答"
echo "  ✓ 流式：SSE → textDelta → 完整应答"

# ---- 图的前门：开着的时候，分类器挂了也必须照常答上来 -----------------------
# 同一份假端点：非流式请求一律 500 —— 分类器和检索门于是都「失败开放」，
# 这一轮落到 full_agent，答案跟不带图时一模一样。
# 这就是那条规矩的端到端版本：图坏了，代价只能是延迟，不能是能力。
PORT2="$(start_fake "$WORK/fake_port2")"
[[ -n "$PORT2" ]] || fail "假 SSE 端点没起来（图那次）"

GRAPH_OUT=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"turn/start","params":{"message":"你好"}}' \
  | JOY_PROVIDER=anthropic JOY_API_KEY=test JOY_BASE_URL="http://127.0.0.1:$PORT2" \
    JOY_GRAPH_WORKFLOWS=1 JOY_HOME="$WORK/.joy-graph" "$BIN" app-server)

echo "$GRAPH_OUT" | sed 's/^/  [graph] /'

echo "$GRAPH_OUT" | grep -q '"type":"graphStarted"'       || fail "开了图却没发 graphStarted"
echo "$GRAPH_OUT" | grep -q '"node":"full_agent"'         || fail "分类器坏了就该落到 full_agent"
echo "$GRAPH_OUT" | grep -q '"type":"graphEnded"'         || fail "图没正常收尾"
echo "$GRAPH_OUT" | grep -q '"route":"full"'              || fail "meta 里该记下走的是 full"
echo "$GRAPH_OUT" | grep -q '"reply":"你好，世界"'         || fail "图开着时也该给出同样的答复"
echo "$GRAPH_OUT" | grep -q '"id":1,"result":{"turnId"'   || fail "图开着时 turn/start 也得正常应答"
echo "  ✓ 图：分类器失败开放 → full_agent → 同样的答复"

echo "✓ 冒烟测试通过"
