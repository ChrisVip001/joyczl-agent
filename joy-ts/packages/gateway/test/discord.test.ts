import assert from "node:assert/strict";
import test from "node:test";

import { CODES, JoyError } from "@joy/client";

import { DiscordAdapter } from "../src/discord.ts";
import type { DiscordSocket, DiscordSocketFactory } from "../src/discord.ts";
import type { Responder } from "../src/bridge.ts";

/** 假的网关连接。真收发帧，只是帧不过网络。 */
class FakeSocket implements DiscordSocket {
  readonly sent: Array<Record<string, unknown>> = [];
  closed = false;

  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;

  send(data: string): void {
    this.sent.push(JSON.parse(data) as Record<string, unknown>);
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.onclose?.();
  }

  /** 服务端推一帧过来。 */
  push(frame: Record<string, unknown>): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }

  /** 发出去的操作码为 `op` 的帧。 */
  frames(op: number): Array<Record<string, unknown>> {
    return this.sent.filter((frame) => frame["op"] === op);
  }
}

class FakeGateway {
  readonly sockets: FakeSocket[] = [];

  readonly factory: DiscordSocketFactory = () => {
    const socket = new FakeSocket();
    this.sockets.push(socket);
    return socket;
  };

  /** 最近那条连接。 */
  get latest(): FakeSocket {
    const socket = this.sockets.at(-1);
    assert.ok(socket !== undefined, "还没有连过");
    return socket;
  }
}

interface SentCall {
  channel: string;
  content: string;
  auth: string | undefined;
  agent: string | undefined;
}

/** 假的 Discord REST。请求的形状跟真的一样。 */
class FakeDiscord {
  readonly calls: SentCall[] = [];
  /** 真发出去的正文（排好队的那几次不算）。 */
  readonly sent: string[] = [];

  readonly #scripted: Array<{ status: number; body?: unknown }> = [];

  /** 排一个应答，按次取；取完之后一律 200。 */
  script(response: { status: number; body?: unknown }): void {
    this.#scripted.push(response);
  }

  readonly fetch = async (
    input: string | URL | Request,
    init?: RequestInit,
  ): Promise<Response> => {
    // 先让出事件循环再回答：真网络每次都得等一个往返，替身照做才忠实。
    await new Promise((resolve) => setImmediate(resolve));

    const body = JSON.parse(String(init?.body ?? "{}")) as { content: string };
    const headers = new Headers(init?.headers);
    this.calls.push({
      channel: String(input).split("/channels/")[1]?.split("/")[0] ?? "",
      content: body.content,
      auth: headers.get("authorization") ?? undefined,
      agent: headers.get("user-agent") ?? undefined,
    });

    const scripted = this.#scripted.shift();
    if (scripted !== undefined) return reply(scripted.status, scripted.body ?? {});
    this.sent.push(body.content);
    return reply(200, {});
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

/** 连接的开场：HELLO 说心跳多久一次。 */
function hello(socket: FakeSocket, heartbeatMs = 60_000): void {
  socket.push({ op: 10, d: { heartbeat_interval: heartbeatMs } });
}

/** 自己是谁。@ 的判定要用它。 */
function ready(socket: FakeSocket, self = { id: "9", username: "joy" }): void {
  socket.push({ op: 0, s: 1, t: "READY", d: { user: self } });
}

function message(
  socket: FakeSocket,
  overrides: {
    channel_id?: string;
    guild_id?: string;
    content?: string;
    author?: { id: string; username?: string; bot?: boolean };
    mentions?: Array<{ id: string }>;
  } = {},
): void {
  socket.push({
    op: 0,
    s: 2,
    t: "MESSAGE_CREATE",
    d: {
      channel_id: "555",
      content: "你好",
      author: { id: "42", username: "alice" },
      ...overrides,
    },
  });
}

function setup(
  options: {
    allow?: readonly string[];
    respond?: Responder;
    onLog?: (text: string) => void;
  } = {},
): { adapter: DiscordAdapter; gateway: FakeGateway; discord: FakeDiscord } {
  const gateway = new FakeGateway();
  const discord = new FakeDiscord();
  const adapter = new DiscordAdapter(
    {
      token: "test-token",
      allow: options.allow,
      fetch: discord.fetch,
      socket: gateway.factory,
      onLog: options.onLog ?? (() => {}),
    },
    options.respond ?? (async () => "答复"),
  );
  return { adapter, gateway, discord };
}

/** 等 `done()` 成立，然后停掉连接并等它真的收尾。 */
async function stopWhen(
  adapter: DiscordAdapter,
  running: Promise<void>,
  done: () => boolean,
  what = "条件",
): Promise<void> {
  const deadline = Date.now() + 5000;
  while (!done()) {
    if (Date.now() > deadline) throw new Error(`等 ${what} 超时了`);
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
  adapter.stop();
  await running;
}

// ---- 连上 ------------------------------------------------------------------

test("连上之后：报上身份，而且该开的事件位一位不少", async () => {
  const { adapter, gateway } = setup();
  const running = adapter.run();
  const socket = gateway.latest;

  hello(socket);

  const identify = socket.frames(2);
  assert.equal(identify.length, 1, "HELLO 之后该报一次身份");
  const payload = identify[0]!["d"] as Record<string, unknown>;
  assert.equal(payload["token"], "test-token");

  // 少一位就收不到该收的东西。MESSAGE_CONTENT 尤其 —— 它是特权位，
  // 开发门户里没打开的话这条连接会被直接掐掉（4013）。
  const intents = payload["intents"] as number;
  for (const [name, bit] of [
    ["GUILDS", 0],
    ["GUILD_MESSAGES", 9],
    ["DIRECT_MESSAGES", 12],
    ["MESSAGE_CONTENT", 15],
  ] as const) {
    assert.equal(intents & (1 << bit), 1 << bit, `少了 ${name}`);
  }

  adapter.stop();
  await running;
});

test("心跳按 HELLO 给的间隔发，带上最近一次的序号", async () => {
  const { adapter, gateway } = setup();
  const running = adapter.run();
  const socket = gateway.latest;

  hello(socket, 10);
  ready(socket);
  message(socket, {});

  await stopWhen(adapter, running, () => socket.frames(1).length > 0, "心跳");

  // 序号得跟着最新那帧走：一直是 null 的话，服务端会当这条连接什么都没收到过。
  assert.equal(socket.frames(1)[0]!["d"], 2);
});

test("stop() 之后 run() 收尾返回，不会自己连回来", async () => {
  const { adapter, gateway } = setup();
  const running = adapter.run();
  hello(gateway.latest);

  adapter.stop();
  await running;

  assert.ok(gateway.latest.closed);
  assert.equal(gateway.sockets.length, 1, "停了就不该再连");
});

test("连都连不上（比如网络不通）：留个痕，退避之后再来", async () => {
  const logs: string[] = [];
  const { adapter, gateway } = setup({ onLog: (text) => logs.push(text) });
  const running = adapter.run();
  const first = gateway.latest;

  // Node 底下连接失败只报这一下，不一定再给 close —— 真机上验过。
  first.onerror?.();

  const deadline = Date.now() + 5000;
  while (gateway.sockets.length < 2) {
    if (Date.now() > deadline) throw new Error("没等到重连");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  adapter.stop();
  await running;

  assert.ok(first.closed, "出错的那条得关掉，不然它的句柄会把进程吊着");
  assert.ok(logs.some((line) => line.includes("连不上")));
});

// ---- 收消息 ----------------------------------------------------------------

test("私聊来一条文字：按频道去问 Joy，把答复发回那个频道", async () => {
  const asked: Array<[string, string]> = [];
  const { adapter, gateway, discord } = setup({
    respond: async (conversation, text) => {
      asked.push([conversation, text]);
      return "你也好";
    },
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket, { channel_id: "555", content: "你好" });

  await stopWhen(adapter, running, () => discord.sent.length > 0, "答复");

  // 会话按**频道**分：私聊一个频道一段对话，服务器里同一频道的人共用一段。
  assert.deepEqual(asked, [["discord:555", "你好"]]);
  assert.deepEqual(discord.sent, ["你也好"]);
});

test("发消息：带上 bot 身份，以及 Discord 要求的 User-Agent", async () => {
  const { adapter, gateway, discord } = setup({ respond: async () => "答复" });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket, { channel_id: "555" });

  await stopWhen(adapter, running, () => discord.calls.length > 0, "发消息");

  const call = discord.calls[0]!;
  assert.equal(call.channel, "555");
  assert.equal(call.auth, "Bot test-token");
  // 这个头缺了或者格式写错，会被挡在 Cloudflare 那一层 —— 报错还看不出原因。
  assert.match(call.agent ?? "", /^DiscordBot \(.+\)$/);
});

test("别的 bot 说的话不算 —— 包括自己刚发出去的那条", async () => {
  let asked = 0;
  const { adapter, gateway } = setup({
    respond: async () => {
      asked += 1;
      return "答复";
    },
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket, { author: { id: "9", username: "joy", bot: true } });
  message(socket, { content: "这句是人说的" });

  await stopWhen(adapter, running, () => asked > 0, "问 Joy");

  assert.equal(asked, 1, "不挡住自己的回话，就会自己跟自己聊起来");
});

test("频道里没叫就不接话；叫了才接，而且那个标记要摘掉", async () => {
  const asked: string[] = [];
  const { adapter, gateway } = setup({
    respond: async (_conversation, text) => {
      asked.push(text);
      return "答复";
    },
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  // 频道里的闲聊，没叫它
  message(socket, { guild_id: "1", content: "今天天气不错" });
  // 叫了它
  message(socket, { guild_id: "1", content: "<@9> 记一下", mentions: [{ id: "9" }] });

  await stopWhen(adapter, running, () => asked.length > 0, "问 Joy");

  assert.deepEqual(asked, ["记一下"], "没叫的闲聊不该问 Joy，@ 的标记也不该带给模型");
});

test("不在白名单里的人：不问 Joy，也不回一句", async () => {
  const logs: string[] = [];
  let asked = 0;
  const { adapter, gateway, discord } = setup({
    allow: ["@alice"],
    respond: async () => {
      asked += 1;
      return "答复";
    },
    onLog: (text) => logs.push(text),
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket, { channel_id: "1", author: { id: "99", username: "mallory" } });
  message(socket, { channel_id: "2", author: { id: "7", username: "alice" } });

  await stopWhen(adapter, running, () => discord.sent.length > 0, "答复");

  assert.equal(asked, 1);
  assert.ok(logs.some((line) => line.includes("不在白名单")));
  assert.equal(discord.calls[0]!.channel, "2", "只该回白名单里那个人");
});

test("白名单里的登录名大小写不敏感", async () => {
  const { adapter, gateway, discord } = setup({
    allow: ["@Alice"],
    respond: async () => "答复",
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket, { author: { id: "7", username: "alice" } });

  await stopWhen(adapter, running, () => discord.sent.length > 0, "答复");

  assert.deepEqual(discord.sent, ["答复"]);
});

test("只发了个附件、没有文字：明说读不了", async () => {
  let asked = 0;
  const { adapter, gateway, discord } = setup({
    respond: async () => {
      asked += 1;
      return "答复";
    },
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket, { content: "" });

  await stopWhen(adapter, running, () => discord.sent.length > 0, "回话");

  assert.equal(asked, 0, "没文字就别去问模型");
  assert.match(discord.sent[0]!, /读不了/);
});

test("Joy 那边出错：把原因说出来，而不是默不作声", async () => {
  const { adapter, gateway, discord } = setup({
    respond: async () => {
      throw new JoyError({ code: CODES.PROVIDER_ERROR, message: "401 Unauthorized" });
    },
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket);

  await stopWhen(adapter, running, () => discord.sent.length > 0, "回话");

  // 用户在聊天框里没有任何别的线索，所以得说清是哪一类问题。
  assert.match(discord.sent[0]!, /模型那边没接上/);
  assert.match(discord.sent[0]!, /joy config/);
});

test("回话发不出去（比如被踢出频道）：只留个痕，网关不能跟着倒", async () => {
  const logs: string[] = [];
  const { adapter, gateway, discord } = setup({
    respond: async () => "答复",
    onLog: (text) => logs.push(text),
  });
  discord.script({ status: 403, body: { message: "Missing Permissions" } });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket);

  // `await running` 能走完，就说明那个拒绝被兜住了 —— 没兜住的话
  // 整个进程会因为一次未捕获的拒绝直接退出。
  await stopWhen(
    adapter,
    running,
    () => logs.some((line) => line.includes("没处理完")),
    "留痕",
  );

  assert.ok(
    logs.some((line) => line.includes("403")),
    "得说清是发不出去，而不是翻成「连不上 Joy」指错方向",
  );
});

// ---- 发出去 ----------------------------------------------------------------

test("回答太长：切成几条发，每条都在上限以内", async () => {
  const line = "这一行特意写长一点，好让总长度确实超过一条消息的上限，看看会不会被硬切。";
  const long = Array.from({ length: 120 }, (_, i) => `${i} ${line}`).join("\n");
  assert.ok(long.length > 2000);

  const { adapter, gateway, discord } = setup({ respond: async () => long });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket);

  await stopWhen(adapter, running, () => discord.sent.length >= 2, "分批发");

  assert.ok(
    discord.sent.every((chunk) => chunk.length <= 2000),
    "超了 Discord 会直接 400，不会替你截",
  );
  assert.equal(discord.sent.join("\n"), long, "切完之后不能少字");
});

test("撞上限流：等它说的那么久再发一次", async () => {
  const { adapter, gateway, discord } = setup({ respond: async () => "答复" });
  discord.script({ status: 429, body: { retry_after: 0.01 } });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket);

  await stopWhen(adapter, running, () => discord.sent.length > 0, "重发");

  assert.equal(discord.calls.length, 2, "第一次 429，第二次才成功");
  assert.deepEqual(discord.sent, ["答复"]);
});

test("连接断了：歇一下自己连回来，而且是重新握手", async () => {
  const logs: string[] = [];
  const { adapter, gateway } = setup({ onLog: (text) => logs.push(text) });
  const running = adapter.run();
  const first = gateway.latest;
  hello(first);
  ready(first);

  // 服务端让它下来的 —— Discord 会定期这么做，这不是故障。
  first.close();

  const deadline = Date.now() + 5000;
  while (gateway.sockets.length < 2) {
    if (Date.now() > deadline) throw new Error("没等到重连");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }

  const second = gateway.latest;
  hello(second);
  // 重新握手，不是 RESUME —— 断的那几秒里说的话就是丢了。这是没实现 RESUME
  // 的代价，钉在这儿，免得以后被当成 bug 查半天。
  assert.equal(second.frames(2).length, 1, "重连得以 IDENTIFY 开场");

  adapter.stop();
  await running;

  assert.ok(
    logs.some((line) => line.includes("连接断了")),
    "断线得留个痕，不然出问题只能靠猜",
  );
});
