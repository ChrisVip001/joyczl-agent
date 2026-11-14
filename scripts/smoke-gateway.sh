#!/usr/bin/env bash
# End-to-end smoke test for the TS gateways: really start an app-server, let a gateway ask it.
#
# In the gateways' unit tests the app-server is a stand-in — that proves queuing, notification
# folding and long-message splitting, but not the real "@joy/client + stdio + app-server" chain.
# This script proves exactly that, above all the key assumption: session ids are **derived** from
# platform identities (`telegram:42`, `wechat:openid`, `lark:oc_…`) the server must own itself.
# The wechat section also proves the "real HTTP server + signature verify + XML reply" half;
# the lark section proves the "private binary frames + ACK echoed verbatim" half.
#
#   ./scripts/smoke-gateway.sh
#
# Requires: a cargo build, joy-ts deps installed (cd joy-ts && npm install), python3.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

BRIDGE="$REPO/joy-ts/packages/gateway/src/bridge.ts"

fail() { echo "✗ $1" >&2; exit 1; }

need_bin

# Fake SSE endpoint: completes the "SSE → full reply" chain without a real key.
# The three chains below each ask once, so it must serve three streaming replies.
python3 "$REPO/scripts/fake_sse_server.py" 3 >"$WORK/port" &
FAKE_PID=$!
track_pid "$FAKE_PID"
for _ in $(seq 1 50); do
  [[ -s "$WORK/port" ]] && break
  sleep 0.1
done
PORT="$(head -n1 "$WORK/port" 2>/dev/null || true)"
[[ -n "$PORT" ]] || fail "the fake SSE endpoint did not come up"

# Models and keys no longer travel via env vars (see 0.7.0): seed a models.json whose
# key_source reads JOY_API_KEY from the environment and whose base_url is the fake SSE
# endpoint. JOY_PROVIDER/JOY_API_KEY below stay — provider is still the fallback vendor,
# and the key is read from the env by the profile above.
mkdir -p "$WORK/.joy"
cat > "$WORK/.joy/models.json" <<EOF
{"active":"smoke","profiles":[{"id":"smoke","provider":"anthropic","base_url":"http://127.0.0.1:$PORT","key_source":"env:JOY_API_KEY"}]}
EOF

# The gateway finds the binary itself; point it here so it doesn't dig up an old version.
OUT="$WORK/out.json"
JOY_BIN="$BIN" \
JOY_HOME="$WORK/.joy" \
JOY_PROVIDER=anthropic JOY_API_KEY=test JOY_BASE_URL="http://127.0.0.1:$PORT" \
JOY_BRIDGE_URL="file://$BRIDGE" \
node --input-type=module -e '
  const { JoyBridge } = await import(process.env.JOY_BRIDGE_URL);
  const bridge = new JoyBridge({ env: process.env });
  const out = await bridge.ask("telegram:42", "hello");
  bridge.close();
  console.log(JSON.stringify(out));
  await new Promise((r) => setTimeout(r, 400));
' > "$OUT"

echo "  the gateway got: $(cat "$OUT")"

# ---- Assertions ------------------------------------------------------------------
grep -q '"sessionId":"telegram:42"' "$OUT" \
  || fail "the derived session id did not come back as it went in — the gateway's statelessness rides on it"
grep -q '"reply":"hello, world"' "$OUT" \
  || fail "the reply is not the full text joined from the notifications"
grep -q '"iterations":1' "$OUT" \
  || fail "the telemetry (round count) was not carried out"
grep -q '"model":"[^"]' "$OUT" \
  || fail "the telemetry carries no model name — a user wants to know who answered"

# The session really hit disk: the server accepted this derived id and created it itself.
[[ -f "$WORK/.joy/state.db" ]] || fail "state.db was not created under .joy/"

echo "  ✓ derived session + real stdio + notifications folded → the full reply"

# ---- The wechat section: a real HTTP server ----------------------------------------------
#
# The section above proves "gateway → app-server", independent of transport; this one adds the
# wechat-specific half — a real HTTP server, real signature verification, real XML parsing and
# reply. WeChat wants a public address, but **this layer doesn't**: hit it on localhost directly.
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

  // The server-configuration step: WeChat verifies this address is yours.
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
    "<Content><![CDATA[hello]]></Content>" +
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

echo "  wechat got: $(cat "$WX_OUT")"

grep -q '"verify":"200 echo-ok"' "$WX_OUT" \
  || fail "URL verification did not echo echostr back verbatim"
grep -q '"status":200' "$WX_OUT" \
  || fail "pushing a message did not get a 200"
grep -q 'wechat:openid-smoke' "$WX_OUT" \
  || fail "the wechat session id has to be derived from the platform identity too"
grep -q 'hello, world' "$WX_OUT" \
  || fail "the reply never reached the passive-reply XML"
grep -q 'ToUserName><!\[CDATA\[openid-smoke' "$WX_OUT" \
  || fail "the reply ToUserName has to be the person who wrote, not the account"

echo "  ✓ real HTTP + signature check + XML parsing → passive reply"

# ---- The lark section: real binary frames ------------------------------------------------
#
# The two sections above prove "gateway → app-server", transport-agnostic; this one adds the
# lark-specific half: trade AppID/AppSecret for a ticketed wss URL, decode **binary frames** into
# events, ACK before answering, and send the reply out over HTTP. Real Lark needs a published
# enterprise app, unavailable locally and in CI, so only the two remote addresses are stubbed
# (fake socket + fake HTTP) — codec, ACK echo, dedup, session derivation, app-server all real.
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

  // The fake connection. It sends/receives with the **real codec** — an undecodable frame blows up.
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
        if (frame === null) throw new Error("the frame we built cannot be taken apart by ourselves");
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

  // The fake Lark HTTP side. These four paths are everything this adapter talks to.
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
    throw new Error("the fake Feishu does not handle this path: " + url);
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

  // Wait for it to connect, and for onopen to be really attached — the first frame is ours.
  for (let i = 0; i < 400; i += 1) {
    if (sockets.length > 0 && sockets[0].onopen !== null) break;
    await sleep(10);
  }
  const socket = sockets[0];
  socket.onopen();
  for (let i = 0; i < 200 && socket.sent.length === 0; i += 1) await sleep(10);

  // A p2p text message, encoded as a real pbbp2 binary frame and pushed over.
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
        content: JSON.stringify({ text: "hello" }),
      },
    },
  });
  socket.onmessage({
    data: encodeFrame({
      seqId: 0n,
      logId: 9007199254740993n,  // beyond 2^53: a wrong backfill is the subtle kind of bug
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

echo "  Feishu got: $(cat "$LARK_OUT")"

grep -qF '"pings":1' "$LARK_OUT" \
  || fail "no heartbeat right after connecting — on this connection the server waits for us to speak first"
grep -qF '"logId":"9007199254740993"' "$LARK_OUT" \
  || fail "the ACK did not echo the server's logId back verbatim (the kind past 2^53)"
grep -qF '"seqId":"0"' "$LARK_OUT" \
  || fail "the ACK seqId should be 0, not one we made up"
grep -qF '"bizRt":"0"' "$LARK_OUT" \
  || fail "the ACK is missing the biz_rt header"
grep -qF 'lark:oc_smoke' "$LARK_OUT" \
  || fail "the Feishu session id has to be derived from the platform identity too"
grep -qF '"said":["hello, world"]' "$LARK_OUT" \
  || fail "the reply was never sent back to Feishu over HTTP"

echo "  ✓ binary frames + ACK echoed + a real app-server → the reply goes back over HTTP"
echo "✓ gateway smoke test passed"
