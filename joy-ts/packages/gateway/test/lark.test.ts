import assert from "node:assert/strict";
import test from "node:test";

import { LarkAdapter } from "../src/lark.ts";
import type { LarkAdapterOptions, LarkSocket, LarkSocketFactory } from "../src/lark.ts";
import { METHOD_CONTROL, METHOD_DATA, decodeFrame, encodeFrame, headerValue } from "../src/lark-proto.ts";
import type { LarkFrame } from "../src/lark-proto.ts";
import type { Responder } from "../src/bridge.ts";

/**
 * The fake connection.
 *
 * It sends and receives **with the real codec** — frames are truly encoded to binary and decoded
 * back, so "a frame we can't decode ourselves" blows up on the spot, not at a real server.
 */
class FakeSocket implements LarkSocket {
  readonly sent: LarkFrame[] = [];
  closed = false;
  binaryType: string | undefined;
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;

  send(data: string | Uint8Array): void {
    assert.ok(data instanceof Uint8Array, "飞书这根线上只发二进制，不发文本");
    const frame = decodeFrame(data);
    assert.ok(frame !== null, "自己编出来的帧自己都解不开");
    this.sent.push(frame);
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.onclose?.();
  }

  /** Connected. The first frame can only go out after this. */
  open(): void {
    this.onopen?.();
  }

  /** The server pushes one frame in. */
  push(frame: LarkFrame | Uint8Array): void {
    this.onmessage?.({ data: frame instanceof Uint8Array ? frame : encodeFrame(frame) });
  }

  /** Push something over that is **not binary**. */
  pushRaw(data: unknown): void {
    this.onmessage?.({ data });
  }

  /** Sent frames whose `type` equals this value. */
  frames(type: string): LarkFrame[] {
    return this.sent.filter((frame) => headerValue(frame, "type") === type);
  }
}

class FakeSockets {
  readonly all: FakeSocket[] = [];

  readonly factory: LarkSocketFactory = () => {
    const socket = new FakeSocket();
    this.all.push(socket);
    return socket;
  };

  get latest(): FakeSocket {
    const socket = this.all.at(-1);
    assert.ok(socket !== undefined, "还没有连过");
    return socket;
  }
}

/** The fake Lark HTTP side. Requests have the same shape as real ones. */
class FakeLark {
  /** The bodies sent out (in order). */
  readonly said: string[] = [];
  readonly asked: string[] = [];
  /** The body of the address-trade request. */
  handshake: unknown;

  pingInterval: number | undefined;
  selfId: string | undefined = "ou_joy";
  /** Script a path's answers, consumed in order. */
  readonly #scripted = new Map<string, Array<{ code: number; msg?: string }>>();

  script(path: string, response: { code: number; msg?: string }): void {
    const queue = this.#scripted.get(path) ?? [];
    queue.push(response);
    this.#scripted.set(path, queue);
  }

  readonly fetch = async (
    input: string | URL | Request,
    init?: RequestInit,
  ): Promise<Response> => {
    // Yield the event loop before answering: a real network always waits a round trip.
    await new Promise((resolve) => setImmediate(resolve));

    const url = String(input);
    this.asked.push(url);
    const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;

    if (url.endsWith("/callback/ws/endpoint")) {
      this.handshake = body;
      return reply(200, {
        code: 0,
        msg: "ok",
        data: {
          URL: `wss://fake.feishu.cn/connect?device_id=d&service_id=1&app_id=${String(body["AppID"])}`,
          ClientConfig: this.pingInterval === undefined ? {} : { PingInterval: this.pingInterval },
        },
      });
    }

    const scripted = this.#scripted.get(new URL(url).pathname)?.shift();
    if (scripted !== undefined) return reply(200, scripted);

    if (url.includes("/auth/v3/tenant_access_token/internal")) {
      return reply(200, { code: 0, tenant_access_token: "t-1", expire: 7200 });
    }
    if (url.includes("/bot/v3/info")) {
      return reply(200, { code: 0, bot: this.selfId === undefined ? {} : { open_id: this.selfId } });
    }
    if (url.includes("/im/v1/messages")) {
      this.said.push(JSON.parse(String(body["content"]))["text"] as string);
      return reply(200, { code: 0 });
    }
    throw new Error(`假飞书没准备这条路径：${url}`);
  };
}

function reply(status: number, value: unknown): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => value,
    text: async () => JSON.stringify(value),
  } as unknown as Response;
}

/**
 * Wait for a condition. The fake network is still async; never assume the next step is done.
 *
 * The deadline is 5 seconds: the reconnect case walks a 2-second backoff, which genuinely waits.
 */
async function waitFor(condition: () => boolean, what = "条件", timeoutMs = 5_000): Promise<void> {
  const until = Date.now() + timeoutMs;
  while (Date.now() < until) {
    if (condition()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  assert.fail(`等不到：${what}`);
}

/** Drain the queued microtasks. */
async function settle(): Promise<void> {
  for (let round = 0; round < 6; round += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
}

interface Harness {
  adapter: LarkAdapter;
  sockets: FakeSockets;
  lark: FakeLark;
  running: Promise<void>;
  /** Everything the adapter said. Some behaviors are only visible in the logs. */
  logs: string[];
  /** Connect and let it start (the first frame must wait for onopen). */
  connect: () => Promise<FakeSocket>;
  stop: () => Promise<void>;
}

function harness(options: Partial<LarkAdapterOptions>, respond: Responder): Harness {
  const sockets = new FakeSockets();
  const lark = new FakeLark();
  const logs: string[] = [];
  const adapter = new LarkAdapter(
    {
      appId: "cli_test",
      appSecret: "secret",
      socket: sockets.factory,
      fetch: lark.fetch,
      onLog: (text) => logs.push(text),
      ...options,
    },
    respond,
  );

  const running = adapter.run();

  return {
    adapter,
    sockets,
    lark,
    running,
    logs,
    connect: async () => {
      await waitFor(() => sockets.all.length > 0, "连上");
      const socket = sockets.latest;
      socket.open();
      await settle();
      return socket;
    },
    stop: async () => {
      adapter.stop();
      await running;
    },
  };
}

/** The JSON of one message event. */
function eventBody(options: {
  openId?: string;
  senderType?: string;
  chatId?: string;
  chatType?: string;
  messageType?: string;
  text?: string;
  mentions?: Array<{ open_id: string; key?: string }>;
  eventType?: string;
}): string {
  return JSON.stringify({
    schema: "2.0",
    header: { event_type: options.eventType ?? "im.message.receive_v1", event_id: "ev_1" },
    event: {
      sender: {
        sender_type: options.senderType ?? "user",
        sender_id: { open_id: options.openId ?? "ou_alice" },
      },
      message: {
        message_id: "om_1",
        chat_id: options.chatId ?? "oc_chat",
        chat_type: options.chatType ?? "p2p",
        message_type: options.messageType ?? "text",
        content: JSON.stringify({ text: options.text ?? "你好" }),
        mentions: (options.mentions ?? []).map((mention, index) => ({
          key: mention.key ?? `@_user_${index + 1}`,
          id: { open_id: mention.open_id },
          name: "某人",
        })),
      },
    },
  });
}

/** One data frame. By default, a p2p text message. */
function dataFrame(
  body: string,
  options: { messageId?: string; sum?: number; seq?: number; logId?: bigint; type?: string } = {},
): LarkFrame {
  const headers = [
    { key: "type", value: options.type ?? "event" },
    { key: "message_id", value: options.messageId ?? "om_1" },
  ];
  if (options.sum !== undefined) headers.push({ key: "sum", value: String(options.sum) });
  if (options.seq !== undefined) headers.push({ key: "seq", value: String(options.seq) });

  return {
    seqId: 0n,
    logId: options.logId ?? 123n,
    service: 1,
    method: METHOD_DATA,
    headers,
    payloadEncoding: "",
    payloadType: "",
    payload: new TextEncoder().encode(body),
    logIdNew: "",
  };
}

/** Split one event into `count` pieces per Lark's fragmentation rule. */
function fragments(body: string, count: number, messageId = "om_1"): LarkFrame[] {
  const bytes = new TextEncoder().encode(body);
  const size = Math.ceil(bytes.length / count);
  return Array.from({ length: count }, (_, seq) => ({
    ...dataFrame("", { messageId, sum: count, seq }),
    payload: bytes.subarray(seq * size, Math.min((seq + 1) * size, bytes.length)),
  }));
}

// ---- Handshake and frames --------------------------------------------------------------

test("换地址：拿 AppID/AppSecret 去要一个有票据的地址", async () => {
  const world = harness({}, async () => "嗯");
  await world.connect();

  assert.deepEqual(world.lark.handshake, { AppID: "cli_test", AppSecret: "secret" });
  // The connection goes to the traded address, not a hardcoded one.
  assert.equal(world.sockets.all.length, 1);

  await world.stop();
});

test("连上就先发心跳，service 是地址里解出来的那个", async () => {
  const world = harness({}, async () => "嗯");
  const socket = await world.connect();

  // The server never speaks first, so this first frame must be ours — and only after onopen.
  const pings = socket.frames("ping");
  assert.equal(pings.length, 1);
  const ping = pings[0];
  assert.ok(ping !== undefined);
  assert.equal(ping.method, METHOD_CONTROL);
  assert.equal(ping.service, 1);
  assert.equal(ping.seqId, 1n);

  await world.stop();
});

/** A control frame whose payload carries the server's new config. */
function pongWith(config: unknown): LarkFrame {
  return {
    seqId: 1n,
    logId: 7n,
    service: 1,
    method: METHOD_CONTROL,
    headers: [{ key: "type", value: "pong" }],
    payloadEncoding: "",
    payloadType: "",
    payload: new TextEncoder().encode(JSON.stringify(config)),
    logIdNew: "",
  };
}

test("心跳按服务端给的间隔发", async (t) => {
  // Swap only setInterval for a fake. setTimeout must stay real — the handshake and the waits
  // depend on it. Actually waiting 120s is unrealistic, and that interval is what's under test.
  t.mock.timers.enable({ apis: ["setInterval"] });

  const world = harness({}, async () => "嗯");
  const socket = await world.connect();
  assert.equal(socket.frames("ping").length, 1, "连上就该先发一次");

  t.mock.timers.tick(120_000);
  assert.equal(socket.frames("ping").length, 2, "默认 120 秒一次");

  await world.stop();
});

test("服务端可以在 pong 里把心跳间隔改掉", async (t) => {
  t.mock.timers.enable({ apis: ["setInterval"] });

  const world = harness({}, async () => "嗯");
  const socket = await world.connect();

  socket.push(pongWith({ ClientConfig: { PingInterval: 20 } }));
  await settle();
  assert.ok(
    world.logs.some((line) => line.includes("20 秒")),
    `日志里该说一句：${world.logs.join(" / ")}`,
  );

  // The new interval really took effect: still on the old 120s, nothing should fire in these 20s.
  t.mock.timers.tick(20_000);
  assert.equal(socket.frames("ping").length, 2);

  await world.stop();
});

test("小于 10 秒的心跳间隔不吃", async (t) => {
  // The official SDK "silently ignores" intervals under 10 seconds; so do we — the server counts
  // its rounds by that number, and beating faster than 10s only scrambles them.
  t.mock.timers.enable({ apis: ["setInterval"] });

  const world = harness({}, async () => "嗯");
  const socket = await world.connect();

  socket.push(pongWith({ ClientConfig: { PingInterval: 1 } }));
  await settle();

  t.mock.timers.tick(1_000);
  assert.equal(socket.frames("ping").length, 1, "1 秒一次不该被采纳");
  t.mock.timers.tick(120_000);
  assert.equal(socket.frames("ping").length, 2, "还是按默认的 120 秒走");

  await world.stop();
});

test("数据帧：先 ACK 再回答，且 ACK 把原帧原样回显", async () => {
  const world = harness({}, async () => "你好，世界");
  const socket = await world.connect();
  const before = socket.sent.length;

  socket.push(dataFrame(eventBody({}), { logId: 9_007_199_254_740_993n }));
  await waitFor(() => world.lark.said.length > 0, "答复");

  const acked = socket.sent.slice(before);
  assert.equal(acked.length, 1, "除了 ACK 不该再往这根连接上写东西 —— 答复走 HTTP");
  const ack = acked[0];
  assert.ok(ack !== undefined);
  // Echo: the ids identical, not one invented by us.
  assert.equal(ack.logId, 9_007_199_254_740_993n);
  assert.equal(ack.seqId, 0n);
  assert.equal(ack.method, METHOD_DATA);
  assert.equal(headerValue(ack, "message_id"), "om_1");
  assert.equal(headerValue(ack, "biz_rt"), "0");
  assert.deepEqual(JSON.parse(new TextDecoder().decode(ack.payload)), {
    code: 200,
    headers: {},
    data: [],
  });

  assert.deepEqual(world.lark.said, ["你好，世界"]);
  await world.stop();
});

test("ACK 不等回答 —— 想多久都不耽误那 3 秒", async () => {
  // Lark's biggest difference from WeChat: acknowledgment and answer are two things. Hold the
  // "thinking" step on purpose — the ACK must already have arrived.
  let release: (() => void) | undefined;
  const thinking = new Promise<void>((resolve) => {
    release = resolve;
  });
  const world = harness({}, async () => {
    await thinking;
    return "想好了";
  });
  const socket = await world.connect();
  const before = socket.sent.length;

  socket.push(dataFrame(eventBody({}), { messageId: "om_slow" }));
  await waitFor(() => socket.sent.length > before, "ACK");

  assert.equal(world.lark.said.length, 0, "这时候还不该有答复");
  release?.();
  await waitFor(() => world.lark.said.length > 0, "答复");
  assert.deepEqual(world.lark.said, ["想好了"]);

  await world.stop();
});

// ---- Fragments ------------------------------------------------------------------

test("分片：凑齐了才处理", async () => {
  const world = harness({}, async () => "齐了");
  const socket = await world.connect();
  const parts = fragments(eventBody({ text: "分片来的" }), 3);
  assert.equal(parts.length, 3);

  socket.push(parts[0] as LarkFrame);
  socket.push(parts[1] as LarkFrame);
  await settle();
  assert.equal(world.lark.said.length, 0, "还差一片，别急着回答");

  socket.push(parts[2] as LarkFrame);
  await waitFor(() => world.lark.said.length > 0, "凑齐之后的答复");
  assert.deepEqual(world.lark.said, ["齐了"]);

  // All three pieces must be ACKed — acknowledgment and completeness are two things.
  assert.equal(socket.frames("event").length, 3);
  await world.stop();
});

test("分片：乱序号或者没给分组键的，丢掉不当整条", async () => {
  // Breaking here is fatal: decoding half a JSON as a whole yields only "event isn't JSON", and
  // you'd never learn the real cause was one large message.
  const world = harness({}, async () => "不该走到这儿");
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({}), { messageId: "", sum: 2, seq: 0 }));
  await settle();
  assert.equal(world.lark.said.length, 0);
  assert.ok(world.logs.some((line) => line.includes("分片帧不对劲")), world.logs.join(" / "));

  await world.stop();
});

// ---- Duplicates and filtering ------------------------------------------------------------

test("同一条重发两遍，只回答一次（但两遍都得 ACK）", async () => {
  const world = harness({}, async () => "只答一次");
  const socket = await world.connect();
  const body = eventBody({});

  socket.push(dataFrame(body, { messageId: "om_dup" }));
  await waitFor(() => world.lark.said.length === 1, "第一次答复");

  socket.push(dataFrame(body, { messageId: "om_dup" }));
  await settle();

  assert.equal(world.lark.said.length, 1, "重发的别再答一遍");
  // Re-sends must be ACKed too: otherwise Lark keeps re-sending forever.
  assert.equal(socket.frames("event").length, 2);
  await world.stop();
});

test("自己发的消息不接（不然就是自问自答）", async () => {
  const world = harness({}, async () => "不该走到这儿");
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({ senderType: "app" })));
  socket.push(dataFrame(eventBody({ senderType: "bot" }), { messageId: "om_bot" }));
  await settle();

  assert.equal(world.lark.said.length, 0);
  await world.stop();
});

test("白名单：不在里头就不理，且说清楚为什么不理", async () => {
  const heard: string[] = [];
  const world = harness({ allow: ["ou_alice"] }, async (_conversation, text) => {
    heard.push(text);
    return "在";
  });
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({ openId: "ou_mallory" })));
  await settle();

  assert.deepEqual(heard, []);
  assert.ok(world.logs.some((line) => line.includes("不在白名单")), world.logs.join(" / "));
  await world.stop();
});

// This also pins the response shape: it passes only if `open_id` is read from the **top-level**
// `bot` (the fake Lark returns the flat on-the-wire `{code, msg, bot}`). The day someone changes
// it to `data.bot`, this goes red immediately.
test("群里：@ 的是别人就不插嘴，@ 到自己才接", async () => {
  const heard: string[] = [];
  const world = harness({}, async (_conversation, text) => {
    heard.push(text);
    return "在";
  });
  const socket = await world.connect();

  socket.push(
    dataFrame(
      eventBody({ chatId: "oc_group", chatType: "group", mentions: [{ open_id: "ou_bob" }] }),
      { messageId: "om_other" },
    ),
  );
  await settle();
  assert.deepEqual(heard, [], "@ 别人不该接话 —— 群里的机器人多嘴一次就该被移出去了");

  socket.push(
    dataFrame(
      eventBody({
        chatId: "oc_group",
        chatType: "group",
        text: "@_user_1 在吗",
        mentions: [{ open_id: "ou_joy", key: "@_user_1" }],
      }),
      { messageId: "om_me" },
    ),
  );
  await waitFor(() => world.lark.said.length > 0, "被 @ 之后的答复");

  // The placeholder must be stripped — that string is noise to the model.
  assert.deepEqual(heard, ["在吗"]);
  await world.stop();
});

test("问不到自己是谁时退回去「有 @ 就算」，且把这件事喊出来", async () => {
  // This fallback is chattier, so the log must say so — "why did it answer someone else" is untraceable.
  const world = harness({}, async () => "在");
  world.lark.selfId = undefined;
  const socket = await world.connect();

  socket.push(
    dataFrame(
      eventBody({ chatId: "oc_group", chatType: "group", mentions: [{ open_id: "ou_bob" }] }),
      { messageId: "om_fallback" },
    ),
  );
  await waitFor(() => world.lark.said.length > 0, "退路生效后的答复");

  assert.ok(
    world.logs.some((line) => line.includes("问不到机器人自己的 open_id")),
    world.logs.join(" / "),
  );
  await world.stop();
});

test("机器人信息被拒时，日志里要带上那个 code", async () => {
  // With bot capability off / unpublished, this API answers "a response, but useless": a non-zero
  // code and no bot. Shouting only "can't fetch open_id" isn't enough — it sends people chasing
  // the network when the cause sits in the developer console.
  const world = harness({}, async () => "在");
  world.lark.script("/open-apis/bot/v3/info", { code: 10_001, msg: "app not enabled" });
  const socket = await world.connect();

  socket.push(
    dataFrame(
      eventBody({ chatId: "oc_group", chatType: "group", mentions: [{ open_id: "ou_bob" }] }),
      { messageId: "om_denied" },
    ),
  );
  await waitFor(() => world.logs.some((line) => line.includes("10001")), "被拒的那条日志");
  await world.stop();
});

test("单聊里不用 @", async () => {
  const world = harness({}, async () => "在");
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({ chatType: "p2p" })));
  await waitFor(() => world.lark.said.length > 0, "答复");
  assert.deepEqual(world.lark.said, ["在"]);
  await world.stop();
});

// ---- Replies ------------------------------------------------------------------

test("读不了的消息（图片之类）明说一句，不是装作没看见", async () => {
  const world = harness({}, async () => "不该走到这儿");
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({ messageType: "image", text: "" })));
  await waitFor(() => world.lark.said.length > 0, "那句话");

  assert.match(world.lark.said[0] ?? "", /读不了/);
  await world.stop();
});

test("答复太长就切成几条发，一条都不丢", async () => {
  const long = "字".repeat(30_001);
  const world = harness({}, async () => long);
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({})));
  await waitFor(() => world.lark.said.length >= 2, "切过之后的两条");

  assert.equal(world.lark.said.length, 2, "30001 个字，30_000 一条，正好两条");
  assert.equal(world.lark.said.join("").length, long.length);
  assert.equal(world.lark.said.join(""), long, "切完拼回来得一模一样");
  await world.stop();
});

// ---- Bad input and reconnects ----------------------------------------------------------

test("认不出的帧丢掉，连接和后面的事都不受影响", async () => {
  const world = harness({}, async () => "还在");
  const socket = await world.connect();

  socket.push(Uint8Array.from([0x08])); // breaks off mid-decode
  socket.pushRaw("纯文本，这根线上不该有");
  await settle();

  assert.ok(world.logs.some((line) => line.includes("认不出一帧")), world.logs.join(" / "));
  assert.ok(
    world.logs.some((line) => line.includes("不是二进制")),
    world.logs.join(" / "),
  );

  // Work continues as usual afterwards.
  socket.push(dataFrame(eventBody({}), { messageId: "om_after" }));
  await waitFor(() => world.lark.said.length > 0, "坏帧之后的答复");
  assert.deepEqual(world.lark.said, ["还在"]);
  await world.stop();
});

test("帧是 Blob 递过来的一定得喊一声", async () => {
  // The sneakiest failure: connected, not one frame decodes, and the log says nothing. The root
  // is WebSocket's default `binaryType` of blob, readable only asynchronously — so this shout is
  // the only clue. Tested with the **first** frame, because the warning fires once per connection.
  const world = harness({}, async () => "嗯");
  const socket = await world.connect();

  socket.pushRaw(new Blob([Uint8Array.from([1, 2])]));
  await settle();

  assert.ok(world.logs.some((line) => line.includes("Blob")), world.logs.join(" / "));
  await world.stop();
});

test("断了会退避重连，而且退避能被 stop() 打断", async () => {
  const world = harness({}, async () => "嗯");
  await world.connect();
  assert.equal(world.sockets.all.length, 1);

  world.sockets.latest.close();
  await settle();
  assert.ok(
    world.logs.some((line) => line.includes("2 秒后重连")),
    `该说清楚等多久：${world.logs.join(" / ")}`,
  );

  // The first backoff is 2 seconds (1000 * 2^1).
  await waitFor(() => world.sockets.all.length === 2, "第二次连接");

  world.sockets.latest.open();
  await settle();
  assert.equal(world.sockets.latest.frames("ping").length, 1, "新连接上照样得先发心跳");

  await world.stop();
});

test("退避中途叫停，不用干等完", async () => {
  // After Ctrl-C the process must exit immediately. Sitting out the 2 seconds would hang scripts.
  const world = harness({}, async () => "嗯");
  await world.connect();
  world.sockets.latest.close();
  await settle();

  const started = Date.now();
  await world.stop();
  assert.ok(Date.now() - started < 1_000, `退避该被打断，却等了 ${Date.now() - started} 毫秒`);
});
