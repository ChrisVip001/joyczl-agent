import assert from "node:assert/strict";
import test from "node:test";

import { LarkAdapter } from "../src/lark.ts";
import type { LarkAdapterOptions, LarkSocket, LarkSocketFactory } from "../src/lark.ts";
import { METHOD_CONTROL, METHOD_DATA, decodeFrame, encodeFrame, headerValue } from "../src/lark-proto.ts";
import type { LarkFrame } from "../src/lark-proto.ts";
import type { Responder } from "../src/bridge.ts";

/**
 * 假的那根连接。
 *
 * 它**用真的编解码**来收发 —— 测试里的帧是实打实编成二进制再解回来的，所以
 * 「发出去的帧自己都认不出」这种事会当场炸，而不是等到连真服务端的时候。
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

  /** 连上了。第一帧得等这个之后才发得出去。 */
  open(): void {
    this.onopen?.();
  }

  /** 服务端推一帧过来。 */
  push(frame: LarkFrame | Uint8Array): void {
    this.onmessage?.({ data: frame instanceof Uint8Array ? frame : encodeFrame(frame) });
  }

  /** 推一帧**不是二进制**的东西过来。 */
  pushRaw(data: unknown): void {
    this.onmessage?.({ data });
  }

  /** 发出去的、`type` 是这个值的帧。 */
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

/** 假的飞书 HTTP 那一侧。请求的形状跟真的一样。 */
class FakeLark {
  /** 发出去的正文（按顺序）。 */
  readonly said: string[] = [];
  readonly asked: string[] = [];
  /** 换地址那次请求的正文。 */
  handshake: unknown;

  pingInterval: number | undefined;
  selfId: string | undefined = "ou_joy";
  /** 让某条路径按脚本回答，按次取。 */
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
    // 先让出事件循环再回答：真网络每次都得等一个往返。
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
 * 等一个条件成立。假的网络也是异步的，不能假设下一步就好了。
 *
 * 期限给到 5 秒：重连那条要走完 2 秒的退避，是真的在等。
 */
async function waitFor(condition: () => boolean, what = "条件", timeoutMs = 5_000): Promise<void> {
  const until = Date.now() + timeoutMs;
  while (Date.now() < until) {
    if (condition()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  assert.fail(`等不到：${what}`);
}

/** 把排着的微任务放完。 */
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
  /** 适配器说过的每一句话。有些行为只能从日志上看出来。 */
  logs: string[];
  /** 连上并叫它开张（第一帧要等 onopen 之后才发得出去）。 */
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

/** 一条消息事件的 JSON。 */
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

/** 一个数据帧。默认就是一条单聊的文本消息。 */
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

/** 把一段事件按飞书的规矩切成 `count` 片。 */
function fragments(body: string, count: number, messageId = "om_1"): LarkFrame[] {
  const bytes = new TextEncoder().encode(body);
  const size = Math.ceil(bytes.length / count);
  return Array.from({ length: count }, (_, seq) => ({
    ...dataFrame("", { messageId, sum: count, seq }),
    payload: bytes.subarray(seq * size, Math.min((seq + 1) * size, bytes.length)),
  }));
}

// ---- 握手与帧 --------------------------------------------------------------

test("换地址：拿 AppID/AppSecret 去要一个有票据的地址", async () => {
  const world = harness({}, async () => "嗯");
  await world.connect();

  assert.deepEqual(world.lark.handshake, { AppID: "cli_test", AppSecret: "secret" });
  // 连的是换来的那个地址，不是写死的。
  assert.equal(world.sockets.all.length, 1);

  await world.stop();
});

test("连上就先发心跳，service 是地址里解出来的那个", async () => {
  const world = harness({}, async () => "嗯");
  const socket = await world.connect();

  // 服务端不会先开口，所以这第一帧必须我们自己发 —— 而且得等 onopen 之后。
  const pings = socket.frames("ping");
  assert.equal(pings.length, 1);
  const ping = pings[0];
  assert.ok(ping !== undefined);
  assert.equal(ping.method, METHOD_CONTROL);
  assert.equal(ping.service, 1);
  assert.equal(ping.seqId, 1n);

  await world.stop();
});

/** 一个控制帧，负载里挂着服务端给的新配置。 */
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
  // 只把 setInterval 换成假的。setTimeout 得留真的 —— 握手和等条件都靠它。
  // 真等 120 秒不现实，而这个间隔正是要验的东西。
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

  // 新间隔真的生效了：还按老的 120 秒的话，这 20 秒里一次都不该发。
  t.mock.timers.tick(20_000);
  assert.equal(socket.frames("ping").length, 2);

  await world.stop();
});

test("小于 10 秒的心跳间隔不吃", async (t) => {
  // 官方 SDK 对小于 10 秒的是「静默忽略」，这里也一样 —— 那个数是服务端按
  // 自己那一轮算的，改得比 10 秒更勤只会把轮次搅乱。
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
  // 回显：编号一模一样，一个都不能自己编。
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
  // 这是飞书跟微信最大的不同：应答和答复是两件事。让「想」这一步故意挂住，
  // ACK 必须已经到了。
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

// ---- 分片 ------------------------------------------------------------------

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

  // 三片都得 ACK —— 应答跟拼不拼得齐是两件事。
  assert.equal(socket.frames("event").length, 3);
  await world.stop();
});

test("分片：乱序号或者没给分组键的，丢掉不当整条", async () => {
  // 坏在这里要命：把半截 JSON 当整条去解，只会得到一句「事件不是 JSON」，
  // 然后你永远不知道真实原因是一条大消息。
  const world = harness({}, async () => "不该走到这儿");
  const socket = await world.connect();

  socket.push(dataFrame(eventBody({}), { messageId: "", sum: 2, seq: 0 }));
  await settle();
  assert.equal(world.lark.said.length, 0);
  assert.ok(world.logs.some((line) => line.includes("分片帧不对劲")), world.logs.join(" / "));

  await world.stop();
});

// ---- 重复与过滤 ------------------------------------------------------------

test("同一条重发两遍，只回答一次（但两遍都得 ACK）", async () => {
  const world = harness({}, async () => "只答一次");
  const socket = await world.connect();
  const body = eventBody({});

  socket.push(dataFrame(body, { messageId: "om_dup" }));
  await waitFor(() => world.lark.said.length === 1, "第一次答复");

  socket.push(dataFrame(body, { messageId: "om_dup" }));
  await settle();

  assert.equal(world.lark.said.length, 1, "重发的别再答一遍");
  // 重发也要应答：不应答的话飞书会一直重发下去。
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

// 顺带钉住响应形状：这条只有在 `open_id` 是从**顶层** `bot` 上读出来的时候
// 才过（假飞书返回的就是线上那个平的 `{code, msg, bot}`）。哪天有人把它改成
// `data.bot`，这里立刻红。
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

  // 占位符得剥掉 —— 那串东西对模型是噪音。
  assert.deepEqual(heard, ["在吗"]);
  await world.stop();
});

test("问不到自己是谁时退回去「有 @ 就算」，且把这件事喊出来", async () => {
  // 这条退路会多嘴，所以日志必须说清楚 —— 不然「它怎么接了别人的话」没人查得动。
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
  // 应用没开机器人能力 / 没发版时，这个接口回的是「有响应但没用」：
  // 非 0 的 code，没有 bot。只喊一句「问不到 open_id」是不够的 ——
  // 那会让人去查网络，而原因在开发者后台。
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

// ---- 答复 ------------------------------------------------------------------

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

// ---- 坏输入与重连 ----------------------------------------------------------

test("认不出的帧丢掉，连接和后面的事都不受影响", async () => {
  const world = harness({}, async () => "还在");
  const socket = await world.connect();

  socket.push(Uint8Array.from([0x08])); // 解到一半就断了
  socket.pushRaw("纯文本，这根线上不该有");
  await settle();

  assert.ok(world.logs.some((line) => line.includes("认不出一帧")), world.logs.join(" / "));
  assert.ok(
    world.logs.some((line) => line.includes("不是二进制")),
    world.logs.join(" / "),
  );

  // 后面照样能干活。
  socket.push(dataFrame(eventBody({}), { messageId: "om_after" }));
  await waitFor(() => world.lark.said.length > 0, "坏帧之后的答复");
  assert.deepEqual(world.lark.said, ["还在"]);
  await world.stop();
});

test("帧是 Blob 递过来的一定得喊一声", async () => {
  // 这是最阴的一种坏法：连上了、一帧都读不出来，日志上却一个字都没有。
  // 根子在 WebSocket 默认的 `binaryType` 是 blob，而 Blob 只能异步读 ——
  // 所以这声喊就是唯一的线索。得拿**第一帧**来测，因为每次连接只喊一次。
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

  // 第一次退避是 2 秒（1000 * 2^1）。
  await waitFor(() => world.sockets.all.length === 2, "第二次连接");

  world.sockets.latest.open();
  await settle();
  assert.equal(world.sockets.latest.frames("ping").length, 1, "新连接上照样得先发心跳");

  await world.stop();
});

test("退避中途叫停，不用干等完", async () => {
  // 按 Ctrl-C 之后要能立刻退出去。等完 2 秒才退出，脚本里就得多挂两秒。
  const world = harness({}, async () => "嗯");
  await world.connect();
  world.sockets.latest.close();
  await settle();

  const started = Date.now();
  await world.stop();
  assert.ok(Date.now() - started < 1_000, `退避该被打断，却等了 ${Date.now() - started} 毫秒`);
});
