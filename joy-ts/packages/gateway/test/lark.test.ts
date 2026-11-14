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
    assert.ok(data instanceof Uint8Array, "only binary travels on the Feishu wire, never text");
    const frame = decodeFrame(data);
    assert.ok(frame !== null, "a frame we built ourselves does not come apart");
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
    assert.ok(socket !== undefined, "it never connected at all");
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
    throw new Error(`the fake Feishu does not handle this path: ${url}`);
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
async function waitFor(condition: () => boolean, what = "the condition", timeoutMs = 5_000): Promise<void> {
  const until = Date.now() + timeoutMs;
  while (Date.now() < until) {
    if (condition()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  assert.fail(`waited in vain for ${what}`);
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
      await waitFor(() => sockets.all.length > 0, "the connection");
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
        content: JSON.stringify({ text: options.text ?? "hello" }),
        mentions: (options.mentions ?? []).map((mention, index) => ({
          key: mention.key ?? `@_user_${index + 1}`,
          id: { open_id: mention.open_id },
          name: "someone",
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

test("exchanging the address: AppID/AppSecret buy an address with a ticket", async () => {
  const world = harness({}, async () => "hm");
  await world.connect();

  assert.deepEqual(world.lark.handshake, { AppID: "cli_test", AppSecret: "secret" });
  // The connection goes to the traded address, not a hardcoded one.
  assert.equal(world.sockets.all.length, 1);

  await world.stop();
});

test("it heartbeats right after connecting, with the service parsed out of the address", async () => {
  const world = harness({}, async () => "hm");
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

test("the heartbeat follows the interval the server gave", async (t) => {
  // Swap only setInterval for a fake. setTimeout must stay real — the handshake and the waits
  // depend on it. Actually waiting 120s is unrealistic, and that interval is what's under test.
  t.mock.timers.enable({ apis: ["setInterval"] });

  const world = harness({}, async () => "hm");
  const socket = await world.connect();
  assert.equal(socket.frames("ping").length, 1, "one right after connecting");

  t.mock.timers.tick(120_000);
  assert.equal(socket.frames("ping").length, 2, "once every 120 s by default");

  await world.stop();
});

test("the server can change the heartbeat interval in a pong", async (t) => {
  t.mock.timers.enable({ apis: ["setInterval"] });

  const world = harness({}, async () => "hm");
  const socket = await world.connect();

  socket.push(pongWith({ ClientConfig: { PingInterval: 20 } }));
  await settle();
  assert.ok(
    world.logs.some((line) => line.includes("20 s")),
    `the log has to say so: ${world.logs.join(" / ")}`,
  );

  // The new interval really took effect: still on the old 120s, nothing should fire in these 20s.
  t.mock.timers.tick(20_000);
  assert.equal(socket.frames("ping").length, 2);

  await world.stop();
});

test("a heartbeat interval under 10 s is not taken", async (t) => {
  // The official SDK "silently ignores" intervals under 10 seconds; so do we — the server counts
  // its rounds by that number, and beating faster than 10s only scrambles them.
  t.mock.timers.enable({ apis: ["setInterval"] });

  const world = harness({}, async () => "hm");
  const socket = await world.connect();

  socket.push(pongWith({ ClientConfig: { PingInterval: 1 } }));
  await settle();

  t.mock.timers.tick(1_000);
  assert.equal(socket.frames("ping").length, 1, "once a second must not be adopted");
  t.mock.timers.tick(120_000);
  assert.equal(socket.frames("ping").length, 2, "it stays on the default 120 s");

  await world.stop();
});

test("a data frame: ACK first, then answer, and the ACK echoes the frame as it came", async () => {
  const world = harness({}, async () => "hello, world");
  const socket = await world.connect();
  const before = socket.sent.length;

  socket.push(dataFrame(eventBody({}), { logId: 9_007_199_254_740_993n }));
  await waitFor(() => world.lark.said.length > 0, "the answer");

  const acked = socket.sent.slice(before);
  assert.equal(acked.length, 1, "nothing but the ACK may go on this connection — the answer goes over HTTP");
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

  assert.deepEqual(world.lark.said, ["hello, world"]);
  await world.stop();
});

test("the ACK does not wait for the answer — thinking as long as it likes costs none of those 3 s", async () => {
  // Lark's biggest difference from WeChat: acknowledgment and answer are two things. Hold the
  // "thinking" step on purpose — the ACK must already have arrived.
  let release: (() => void) | undefined;
  const thinking = new Promise<void>((resolve) => {
    release = resolve;
  });
  const world = harness({}, async () => {
    await thinking;
    return "thought it over";
  });
  const socket = await world.connect();
  const before = socket.sent.length;

  socket.push(dataFrame(eventBody({}), { messageId: "om_slow" }));
  await waitFor(() => socket.sent.length > before, "ACK");

  assert.equal(world.lark.said.length, 0, "there must be no answer yet");
  release?.();
  await waitFor(() => world.lark.said.length > 0, "the answer");
  assert.deepEqual(world.lark.said, ["thought it over"]);

  await world.stop();
});

// ---- Fragments ------------------------------------------------------------------

test("fragments: handled only once they add up", async () => {
  const world = harness({}, async () => "all here");
  const socket = await world.connect();
  const parts = fragments(eventBody({ text: "this came in pieces" }), 3);
  assert.equal(parts.length, 3);

  socket.push(parts[0] as LarkFrame);
  socket.push(parts[1] as LarkFrame);
  await settle();
  assert.equal(world.lark.said.length, 0, "one piece short, so hold the answer");

  socket.push(parts[2] as LarkFrame);
  await waitFor(() => world.lark.said.length > 0, "the answer once it all arrived");
  assert.deepEqual(world.lark.said, ["all here"]);

  // All three pieces must be ACKed — acknowledgment and completeness are two things.
  assert.equal(socket.frames("event").length, 3);
  await world.stop();
});

test("fragments: a wrong sequence number, or no group key, drops it instead of treating it as whole", async () => {
  // Breaking here is fatal: decoding half a JSON as a whole yields only "event isn't JSON", and
  // you'd never learn the real cause was one large message.
  const world = harness({}, async () => "this should never run");
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({}), { messageId: "", sum: 2, seq: 0 }));
  await settle();
  assert.equal(world.lark.said.length, 0);
  assert.ok(world.logs.some((line) => line.includes("does not add up")), world.logs.join(" / "));

  await world.stop();
});

// ---- Duplicates and filtering ------------------------------------------------------------

test("the same message resent twice is answered once (but both get an ACK)", async () => {
  const world = harness({}, async () => "answered once");
  const socket = await world.connect();
  const body = eventBody({});

  socket.push(dataFrame(body, { messageId: "om_dup" }));
  await waitFor(() => world.lark.said.length === 1, "the first answer");

  socket.push(dataFrame(body, { messageId: "om_dup" }));
  await settle();

  assert.equal(world.lark.said.length, 1, "the resend must not be answered again");
  // Re-sends must be ACKed too: otherwise Lark keeps re-sending forever.
  assert.equal(socket.frames("event").length, 2);
  await world.stop();
});

test("its own messages are not taken (or it answers itself)", async () => {
  const world = harness({}, async () => "this should never run");
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({ senderType: "app" })));
  socket.push(dataFrame(eventBody({ senderType: "bot" }), { messageId: "om_bot" }));
  await settle();

  assert.equal(world.lark.said.length, 0);
  await world.stop();
});

test("the allowlist: outside it means no answer, and the log says why", async () => {
  const heard: string[] = [];
  const world = harness({ allow: ["ou_alice"] }, async (_conversation, text) => {
    heard.push(text);
    return "here";
  });
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({ openId: "ou_mallory" })));
  await settle();

  assert.deepEqual(heard, []);
  assert.ok(world.logs.some((line) => line.includes("outside the allowlist")), world.logs.join(" / "));
  await world.stop();
});

// This also pins the response shape: it passes only if `open_id` is read from the **top-level**
// `bot` (the fake Lark returns the flat on-the-wire `{code, msg, bot}`). The day someone changes
// it to `data.bot`, this goes red immediately.
test("in a group: an @ to someone else is not its business, an @ to itself is", async () => {
  const heard: string[] = [];
  const world = harness({}, async (_conversation, text) => {
    heard.push(text);
    return "here";
  });
  const socket = await world.connect();

  socket.push(
    dataFrame(
      eventBody({ chatId: "oc_group", chatType: "group", mentions: [{ open_id: "ou_bob" }] }),
      { messageId: "om_other" },
    ),
  );
  await settle();
  assert.deepEqual(heard, [], "an @ to someone else must not be answered — a bot that talks out of turn gets removed");

  socket.push(
    dataFrame(
      eventBody({
        chatId: "oc_group",
        chatType: "group",
        text: "@_user_1 are you there",
        mentions: [{ open_id: "ou_joy", key: "@_user_1" }],
      }),
      { messageId: "om_me" },
    ),
  );
  await waitFor(() => world.lark.said.length > 0, "the answer after the @");

  // The placeholder must be stripped — that string is noise to the model.
  assert.deepEqual(heard, ["are you there"]);
  await world.stop();
});

test("when it cannot learn who it is, «an @ counts» is the fallback, and the log says so loudly", async () => {
  // This fallback is chattier, so the log must say so — "why did it answer someone else" is untraceable.
  const world = harness({}, async () => "here");
  world.lark.selfId = undefined;
  const socket = await world.connect();

  socket.push(
    dataFrame(
      eventBody({ chatId: "oc_group", chatType: "group", mentions: [{ open_id: "ou_bob" }] }),
      { messageId: "om_fallback" },
    ),
  );
  await waitFor(() => world.lark.said.length > 0, "the answer once the fallback kicks in");

  assert.ok(
    world.logs.some((line) => line.includes("cannot ask for the bot's own open_id")),
    world.logs.join(" / "),
  );
  await world.stop();
});

test("when the bot info is refused, the log has to carry that code", async () => {
  // With bot capability off / unpublished, this API answers "a response, but useless": a non-zero
  // code and no bot. Shouting only "can't fetch open_id" isn't enough — it sends people chasing
  // the network when the cause sits in the developer console.
  const world = harness({}, async () => "here");
  world.lark.script("/open-apis/bot/v3/info", { code: 10_001, msg: "app not enabled" });
  const socket = await world.connect();

  socket.push(
    dataFrame(
      eventBody({ chatId: "oc_group", chatType: "group", mentions: [{ open_id: "ou_bob" }] }),
      { messageId: "om_denied" },
    ),
  );
  await waitFor(() => world.logs.some((line) => line.includes("10001")), "the refusal line");
  await world.stop();
});

test("a direct chat needs no @", async () => {
  const world = harness({}, async () => "here");
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({ chatType: "p2p" })));
  await waitFor(() => world.lark.said.length > 0, "the answer");
  assert.deepEqual(world.lark.said, ["here"]);
  await world.stop();
});

// ---- Replies ------------------------------------------------------------------

test("an unreadable message (an image, say) gets a plain word, not silent ignorance", async () => {
  const world = harness({}, async () => "this should never run");
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({ messageType: "image", text: "" })));
  await waitFor(() => world.lark.said.length > 0, "that sentence");

  assert.match(world.lark.said[0] ?? "", /cannot read this one/);
  await world.stop();
});

test("an answer that is too long goes out in several, losing nothing", async () => {
  const long = "x".repeat(30_001);
  const world = harness({}, async () => long);
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({})));
  await waitFor(() => world.lark.said.length >= 2, "the two after the split");

  assert.equal(world.lark.said.length, 2, "30001 characters, 30_000 each, exactly two");
  assert.equal(world.lark.said.join("").length, long.length);
  assert.equal(world.lark.said.join(""), long, "joined back it has to be identical");
  await world.stop();
});

// ---- Bad input and reconnects ----------------------------------------------------------

test("an unrecognised frame is dropped, and neither the connection nor what follows is affected", async () => {
  const world = harness({}, async () => "still here");
  const socket = await world.connect();

  socket.push(Uint8Array.from([0x08])); // breaks off mid-decode
  socket.pushRaw("plain text, which has no business on this wire");
  await settle();

  assert.ok(world.logs.some((line) => line.includes("could not be recognised")), world.logs.join(" / "));
  assert.ok(
    world.logs.some((line) => line.includes("not binary")),
    world.logs.join(" / "),
  );

  // Work continues as usual afterwards.
  socket.push(dataFrame(eventBody({}), { messageId: "om_after" }));
  await waitFor(() => world.lark.said.length > 0, "the answer after the bad frame");
  assert.deepEqual(world.lark.said, ["still here"]);
  await world.stop();
});

test("frames arriving as Blobs have to be shouted about", async () => {
  // The sneakiest failure: connected, not one frame decodes, and the log says nothing. The root
  // is WebSocket's default `binaryType` of blob, readable only asynchronously — so this shout is
  // the only clue. Tested with the **first** frame, because the warning fires once per connection.
  const world = harness({}, async () => "hm");
  const socket = await world.connect();

  socket.pushRaw(new Blob([Uint8Array.from([1, 2])]));
  await settle();

  assert.ok(world.logs.some((line) => line.includes("Blob")), world.logs.join(" / "));
  await world.stop();
});

test("a drop backs off and reconnects, and the backoff can be cut short by stop()", async () => {
  const world = harness({}, async () => "hm");
  await world.connect();
  assert.equal(world.sockets.all.length, 1);

  world.sockets.latest.close();
  await settle();
  assert.ok(
    world.logs.some((line) => line.includes("reconnecting in 2 s")),
    `it has to say how long the wait is: ${world.logs.join(" / ")}`,
  );

  // The first backoff is 2 seconds (1000 * 2^1).
  await waitFor(() => world.sockets.all.length === 2, "the second connection");

  world.sockets.latest.open();
  await settle();
  assert.equal(world.sockets.latest.frames("ping").length, 1, "a new connection still heartbeats first");

  await world.stop();
});

test("stopping mid-backoff does not wait it out", async () => {
  // After Ctrl-C the process must exit immediately. Sitting out the 2 seconds would hang scripts.
  const world = harness({}, async () => "hm");
  await world.connect();
  world.sockets.latest.close();
  await settle();

  const started = Date.now();
  await world.stop();
  assert.ok(Date.now() - started < 1_000, `the backoff should have been cut short, but it waited ${Date.now() - started} ms`);
});
