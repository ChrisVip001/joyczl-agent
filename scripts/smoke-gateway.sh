#!/usr/bin/env bash
# TypeScript 网关的端到端冒烟测试：真的起一个 app-server，让网关问它一句。
#
# 网关的单测里，app-server 是个替身 —— 那能证明排队、通知归拢、切长消息这些
# 逻辑对，但证明不了「@joy/client + stdio + app-server」这条真链路通。这个脚本
# 就干这件事，尤其是网关最要紧的那个假设：会话 id 从平台身份**派生**出来
# （`telegram:42`、`wechat:openid`、`lark:oc_…`），服务端得自己认它、自己建它。
# 微信那条还顺带证明「真 HTTP 服务器 + 验签 + XML 回包」这半截也是通的；
# 飞书那条顺带证明「私有二进制帧 + ACK 原样回显」这半截也是通的。
#
#   ./scripts/smoke-gateway.sh
#
# 需要：cargo build 过、joy-ts 装过依赖（cd joy-ts && npm install）、python3。

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/joy-rs/target/debug/joy"
BRIDGE="$REPO/joy-ts/packages/gateway/src/bridge.ts"
WORK="$(mktemp -d)"
FAKE_PID=""
cleanup() {
  [[ -n "$FAKE_PID" ]] && kill "$FAKE_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "✗ $1" >&2; exit 1; }

[[ -x "$BIN" ]] || fail "找不到 $BIN，先跑：cd joy-rs && cargo build"

# 假的 SSE 端点：不用真 key 也能把「SSE → 完整答复」这条链走完。
# 下面三条链各要问一句，所以要它服务三轮流式应答。
python3 "$REPO/scripts/fake_sse_server.py" 3 >"$WORK/port" &
FAKE_PID=$!
for _ in $(seq 1 50); do
  [[ -s "$WORK/port" ]] && break
  sleep 0.1
done
PORT="$(head -n1 "$WORK/port" 2>/dev/null || true)"
[[ -n "$PORT" ]] || fail "假 SSE 端点没起来"

# 网关自己去找二进制，这里直接指过去，免得它从别处翻出来一个旧版本。
OUT="$WORK/out.json"
JOY_BIN="$BIN" \
JOY_HOME="$WORK/.joy" \
JOY_PROVIDER=anthropic JOY_API_KEY=test JOY_BASE_URL="http://127.0.0.1:$PORT" \
JOY_BRIDGE_URL="file://$BRIDGE" \
node --input-type=module -e '
  const { JoyBridge } = await import(process.env.JOY_BRIDGE_URL);
  const bridge = new JoyBridge({ env: process.env });
  const out = await bridge.ask("telegram:42", "你好");
  bridge.close();
  console.log(JSON.stringify(out));
  await new Promise((r) => setTimeout(r, 400));
' > "$OUT"

echo "  网关拿到：$(cat "$OUT")"

# ---- 断言 ------------------------------------------------------------------
grep -q '"sessionId":"telegram:42"' "$OUT" \
  || fail "派生的会话 id 没原样回来 —— 网关的「无状态」就靠它"
grep -q '"reply":"你好，世界"' "$OUT" \
  || fail "答复不是通知串拼起来的完整文本"
grep -q '"iterations":1' "$OUT" \
  || fail "遥测（轮数）没带出来"
grep -q '"model":"[^"]' "$OUT" \
  || fail "遥测里没有模型名 —— 用户会想知道到底谁答的"

# 会话真的落了盘：服务端认下了这个派生的 id，自己建了它。
[[ -f "$WORK/.joy/state.db" ]] || fail "state.db 没建在 .joy/ 下"

echo "  ✓ 派生会话 + 真 stdio + 通知归拢 → 完整答复"

# ---- 微信这条：真的 HTTP 服务器 ----------------------------------------------
#
# 上面那段证明的是「网关 → app-server」，跟传输是哪家无关；这一段补的是微信
# 特有的那一半 —— 真的 HTTP 服务器、真的验签、真的 XML 解析与回包。微信要求
# 公网地址，但**这一层不用**：在本机直接打它就是。
WX_PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
WX_OUT="$WORK/wechat.json"
WECHAT_TOKEN="smoke-token" \
WECHAT_APP_ID="wxsmoke0000000000" \
WECHAT_PORT="$WX_PORT" \
JOY_BIN="$BIN" \
JOY_HOME="$WORK/.joy" \
JOY_PROVIDER=anthropic JOY_API_KEY=test JOY_BASE_URL="http://127.0.0.1:$PORT" \
JOY_BRIDGE_URL="file://$BRIDGE" \
JOY_WECHAT_URL="file://$REPO/joy-ts/packages/gateway/src/wechat.ts" \
node --input-type=module -e '
  const { createHash } = await import("node:crypto");
  const { JoyBridge } = await import(process.env.JOY_BRIDGE_URL);
  const { WeChatAdapter } = await import(process.env.JOY_WECHAT_URL);

  const bridge = new JoyBridge({ env: process.env });
  const seen = [];
  const adapter = new WeChatAdapter(
    {
      token: process.env.WECHAT_TOKEN,
      appId: process.env.WECHAT_APP_ID,
      port: Number(process.env.WECHAT_PORT),
      onLog: (text) => console.error("  [wechat] " + text),
    },
    async (conversation, text) => {
      seen.push(conversation + "|" + text);
      return (await bridge.ask(conversation, text)).reply;
    },
  );

  const running = adapter.run();
  const timestamp = "1714112445";
  const nonce = "415670741";
  const signature = createHash("sha1")
    .update([process.env.WECHAT_TOKEN, timestamp, nonce].sort().join(""))
    .digest("hex");
  const query = new URLSearchParams({ signature, timestamp, nonce });
  const base = "http://127.0.0.1:" + process.env.WECHAT_PORT + "/wechat";

  // 服务器配置那一步：微信来验这个地址是不是你的。
  let verify;
  for (let attempt = 0; attempt < 100; attempt += 1) {
    try {
      verify = await fetch(base + "?" + new URLSearchParams({ ...Object.fromEntries(query), echostr: "echo-ok" }));
      break;
    } catch {
      await new Promise((r) => setTimeout(r, 50));
    }
  }

  const xml =
    "<xml>" +
    "<ToUserName><![CDATA[gh_smoke]]></ToUserName>" +
    "<FromUserName><![CDATA[openid-smoke]]></FromUserName>" +
    "<CreateTime>1713424427</CreateTime>" +
    "<MsgType><![CDATA[text]]></MsgType>" +
    "<Content><![CDATA[你好]]></Content>" +
    "<MsgId>1</MsgId>" +
    "</xml>";
  const reply = await fetch(base + "?" + query, { method: "POST", body: xml });

  console.log(
    JSON.stringify({
      verify: verify.status + " " + (await verify.text()),
      status: reply.status,
      body: await reply.text(),
      seen,
    }),
  );

  adapter.stop();
  await running;
  bridge.close();
  await new Promise((r) => setTimeout(r, 400));
' > "$WX_OUT"

echo "  微信拿到：$(cat "$WX_OUT")"

grep -q '"verify":"200 echo-ok"' "$WX_OUT" \
  || fail "URL 验证没把 echostr 原样回过去"
grep -q '"status":200' "$WX_OUT" \
  || fail "推一条消息没拿到 200"
grep -q 'wechat:openid-smoke' "$WX_OUT" \
  || fail "微信这条的会话 id 也得从平台身份派生"
grep -q '你好，世界' "$WX_OUT" \
  || fail "答复没进被动回复的 XML"
grep -q 'ToUserName><!\[CDATA\[openid-smoke' "$WX_OUT" \
  || fail "回包的 ToUserName 得是发消息的那个人，不是公众号"

echo "  ✓ 真 HTTP + 验签 + XML 解析 → 被动回复"

# ---- 飞书这条：真的二进制帧 ------------------------------------------------
#
# 上面两条证明「网关 → app-server」跟传输是哪家无关；这一段补飞书特有的那半截：
# 拿 AppID/AppSecret 换一个带票据的 wss 地址、把**二进制帧**解成事件、先 ACK
# 再回答、最后由 HTTP 把答复发出去。真飞书要一个已发布的企业应用，本地和 CI
# 都拿不到，所以打桩的只有远端那两个地址（假 socket + 假 HTTP）——
# 中间的编解码、ACK 回显、去重、会话派生、真 app-server 全是真跑的。
LARK_OUT="$WORK/lark.json"
JOY_BIN="$BIN" \
JOY_HOME="$WORK/.joy" \
JOY_PROVIDER=anthropic JOY_API_KEY=test JOY_BASE_URL="http://127.0.0.1:$PORT" \
JOY_BRIDGE_URL="file://$BRIDGE" \
JOY_LARK_URL="file://$REPO/joy-ts/packages/gateway/src/lark.ts" \
JOY_LARK_PROTO_URL="file://$REPO/joy-ts/packages/gateway/src/lark-proto.ts" \
node --input-type=module -e '
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const { JoyBridge } = await import(process.env.JOY_BRIDGE_URL);
  const { LarkAdapter } = await import(process.env.JOY_LARK_URL);
  const { decodeFrame, encodeFrame, headerValue } = await import(process.env.JOY_LARK_PROTO_URL);

  const bridge = new JoyBridge({ env: process.env });
  const seen = [];
  const said = [];
  const sockets = [];

  // 假的那根连接。它用**真的编解码**收发 —— 发出去的帧自己解不开就会当场炸。
  const openSocket = () => {
    const socket = {
      sent: [],
      binaryType: undefined,
      onopen: null,
      onmessage: null,
      onclose: null,
      onerror: null,
      send(data) {
        const frame = decodeFrame(data);
        if (frame === null) throw new Error("编出来的帧自己解不开");
        socket.sent.push(frame);
      },
      close() { socket.onclose?.(); },
    };
    sockets.push(socket);
    return socket;
  };

  const reply = (value) => ({
    ok: true, status: 200,
    json: async () => value, text: async () => JSON.stringify(value),
  });

  // 假的飞书 HTTP 那一侧。四条路径就是这一家全部要说话的地方。
  const fakeFetch = async (input, init) => {
    const url = String(input);
    if (url.endsWith("/callback/ws/endpoint")) {
      return reply({ code: 0, msg: "ok", data: {
        URL: "wss://fake.feishu.cn/connect?device_id=d&service_id=1",
        ClientConfig: { PingInterval: 120 },
      }});
    }
    if (url.includes("/auth/v3/tenant_access_token/internal")) {
      return reply({ code: 0, tenant_access_token: "t-smoke", expire: 7200 });
    }
    if (url.includes("/bot/v3/info")) {
      return reply({ code: 0, bot: { open_id: "ou_joy", app_name: "Joy" } });
    }
    if (url.includes("/im/v1/messages")) {
      said.push(JSON.parse(JSON.parse(String(init.body)).content).text);
      return reply({ code: 0 });
    }
    throw new Error("假飞书没准备这条路径：" + url);
  };

  const adapter = new LarkAdapter(
    {
      appId: "cli_smoke",
      appSecret: "smoke-secret",
      socket: openSocket,
      fetch: fakeFetch,
      onLog: (text) => console.error("  [lark] " + text),
    },
    async (conversation, text) => {
      seen.push(conversation + "|" + text);
      return (await bridge.ask(conversation, text)).reply;
    },
  );

  const running = adapter.run();

  // 等它连上，而且得等 onopen 真的挂上去 —— 第一帧是我们先发的。
  for (let i = 0; i < 400; i += 1) {
    if (sockets.length > 0 && sockets[0].onopen !== null) break;
    await sleep(10);
  }
  const socket = sockets[0];
  socket.onopen();
  for (let i = 0; i < 200 && socket.sent.length === 0; i += 1) await sleep(10);

  // 一条单聊文本消息，编成真的 pbbp2 二进制帧推过去。
  const body = JSON.stringify({
    schema: "2.0",
    header: { event_type: "im.message.receive_v1", event_id: "ev_smoke" },
    event: {
      sender: { sender_type: "user", sender_id: { open_id: "ou_alice" } },
      message: {
        message_id: "om_smoke",
        chat_id: "oc_smoke",
        chat_type: "p2p",
        message_type: "text",
        content: JSON.stringify({ text: "你好" }),
      },
    },
  });
  socket.onmessage({
    data: encodeFrame({
      seqId: 0n,
      logId: 9007199254740993n,  // 超过 2^53：回填错了就是那种平时看不出的错
      service: 1,
      method: 1,
      headers: [
        { key: "type", value: "event" },
        { key: "message_id", value: "om_smoke" },
      ],
      payloadEncoding: "",
      payloadType: "",
      payload: new TextEncoder().encode(body),
      logIdNew: "",
    }),
  });

  for (let i = 0; i < 400 && said.length === 0; i += 1) await sleep(50);

  const ack = socket.sent.find((frame) => headerValue(frame, "type") === "event");
  console.log(JSON.stringify({
    pings: socket.sent.filter((frame) => headerValue(frame, "type") === "ping").length,
    acked: ack === undefined ? null : {
      logId: String(ack.logId),
      seqId: String(ack.seqId),
      messageId: headerValue(ack, "message_id"),
      bizRt: headerValue(ack, "biz_rt"),
    },
    said,
    seen,
  }));

  adapter.stop();
  await running;
  bridge.close();
  await sleep(400);
' > "$LARK_OUT"

echo "  飞书拿到：$(cat "$LARK_OUT")"

grep -qF '"pings":1' "$LARK_OUT" \
  || fail "连上之后没先发心跳 —— 这根连接是服务端等我们先开口的"
grep -qF '"logId":"9007199254740993"' "$LARK_OUT" \
  || fail "ACK 没把服务端给的 logId 原样回填（超过 2^53 的那种）"
grep -qF '"seqId":"0"' "$LARK_OUT" \
  || fail "ACK 的 seqId 该回 0，不是自己编一个"
grep -qF '"bizRt":"0"' "$LARK_OUT" \
  || fail "ACK 少了 biz_rt 头"
grep -qF 'lark:oc_smoke' "$LARK_OUT" \
  || fail "飞书这条的会话 id 也得从平台身份派生"
grep -qF '"said":["你好，世界"]' "$LARK_OUT" \
  || fail "答复没经 HTTP 发回飞书"

echo "  ✓ 二进制帧 + ACK 回显 + 真 app-server → HTTP 发回答复"
echo "✓ 网关冒烟测试通过"
