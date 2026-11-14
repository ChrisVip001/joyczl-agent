import assert from "node:assert/strict";
import test from "node:test";

import { CODES, JoyError } from "@joy/client";

import { DiscordAdapter } from "../src/discord.ts";
import type { DiscordSocket, DiscordSocketFactory } from "../src/discord.ts";
import type { Responder } from "../src/bridge.ts";

/** A fake gateway connection. Frames really go back and forth; they just never touch a network. */
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

  /** The server pushes one frame in. */
  push(frame: Record<string, unknown>): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }

/** Frames sent with opcode `op`. */
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

  /** The latest connection. */
  get latest(): FakeSocket {
    const socket = this.sockets.at(-1);
    assert.ok(socket !== undefined, "it never connected at all");
    return socket;
  }
}

interface SentCall {
  channel: string;
  content: string;
  auth: string | undefined;
  agent: string | undefined;
}

/** A fake Discord REST. Requests have the same shape as real ones. */
class FakeDiscord {
  readonly calls: SentCall[] = [];
  /** The bodies really sent (scripted responses don't count). */
  readonly sent: string[] = [];

  readonly #scripted: Array<{ status: number; body?: unknown }> = [];

  /** Queue one response, consumed in order; once exhausted, always 200. */
  script(response: { status: number; body?: unknown }): void {
    this.#scripted.push(response);
  }

  readonly fetch = async (
    input: string | URL | Request,
    init?: RequestInit,
  ): Promise<Response> => {
    // Yield the event loop before answering: a real network always waits a round trip; a faithful double does too.
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

/** The connection's opening: HELLO says how often to heartbeat. */
function hello(socket: FakeSocket, heartbeatMs = 60_000): void {
  socket.push({ op: 10, d: { heartbeat_interval: heartbeatMs } });
}

/** Who we are. Needed for @-mention detection. */
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
      content: "hello",
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
    options.respond ?? (async () => "the answer"),
  );
  return { adapter, gateway, discord };
}

/** Wait for `done()`, then stop the connection and await its real shutdown. */
async function stopWhen(
  adapter: DiscordAdapter,
  running: Promise<void>,
  done: () => boolean,
  what = "the condition",
): Promise<void> {
  const deadline = Date.now() + 5000;
  while (!done()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
  adapter.stop();
  await running;
}

// ---- Connecting ------------------------------------------------------------------

test("once connected: it identifies itself, and every event bit it needs is on", async () => {
  const { adapter, gateway } = setup();
  const running = adapter.run();
  const socket = gateway.latest;

  hello(socket);

  const identify = socket.frames(2);
  assert.equal(identify.length, 1, "one IDENTIFY after HELLO");
  const payload = identify[0]!["d"] as Record<string, unknown>;
  assert.equal(payload["token"], "test-token");

  // One bit short and the matching events never arrive. MESSAGE_CONTENT especially — it's a
  // privileged bit; without the portal toggle the connection is cut outright (4013).
  const intents = payload["intents"] as number;
  for (const [name, bit] of [
    ["GUILDS", 0],
    ["GUILD_MESSAGES", 9],
    ["DIRECT_MESSAGES", 12],
    ["MESSAGE_CONTENT", 15],
  ] as const) {
    assert.equal(intents & (1 << bit), 1 << bit, `missing ${name}`);
  }

  adapter.stop();
  await running;
});

test("the heartbeat follows the interval HELLO gave, carrying the latest sequence number", async () => {
  const { adapter, gateway } = setup();
  const running = adapter.run();
  const socket = gateway.latest;

  hello(socket, 10);
  ready(socket);
  message(socket, {});

  await stopWhen(adapter, running, () => socket.frames(1).length > 0, "a heartbeat");

  // The sequence must track the latest frame: always null, and the server thinks nothing was ever received.
  assert.equal(socket.frames(1)[0]!["d"], 2);
});

test("after stop() the run winds up and does not come back on its own", async () => {
  const { adapter, gateway } = setup();
  const running = adapter.run();
  hello(gateway.latest);

  adapter.stop();
  await running;

  assert.ok(gateway.latest.closed);
  assert.equal(gateway.sockets.length, 1, "once stopped it must not reconnect");
});

test("it cannot even connect (no network, say): leave a trace, then try again after a backoff", async () => {
  const logs: string[] = [];
  const { adapter, gateway } = setup({ onLog: (text) => logs.push(text) });
  const running = adapter.run();
  const first = gateway.latest;

  // Under Node a failed connect reports only this, not necessarily a close — verified on real machines.
  first.onerror?.();

  const deadline = Date.now() + 5000;
  while (gateway.sockets.length < 2) {
    if (Date.now() > deadline) throw new Error("the reconnect never came");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  adapter.stop();
  await running;

  assert.ok(first.closed, "the failed one has to be closed, or its handle keeps the process alive");
  assert.ok(logs.some((line) => line.includes("cannot connect")));
});

// ---- Receiving messages ----------------------------------------------------------------

test("a direct message arrives: ask Joy by channel, and send the answer back to that channel", async () => {
  const asked: Array<[string, string]> = [];
  const { adapter, gateway, discord } = setup({
    respond: async (conversation, text) => {
      asked.push([conversation, text]);
      return "hello to you too";
    },
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket, { channel_id: "555", content: "hello" });

  await stopWhen(adapter, running, () => discord.sent.length > 0, "the answer");

  // Sessions split by **channel**: one conversation per DM channel; a server channel shares one.
  assert.deepEqual(asked, [["discord:555", "hello"]]);
  assert.deepEqual(discord.sent, ["hello to you too"]);
});

test("sending a message: it carries the bot identity and the User-Agent Discord asks for", async () => {
  const { adapter, gateway, discord } = setup({ respond: async () => "the answer" });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket, { channel_id: "555" });

  await stopWhen(adapter, running, () => discord.calls.length > 0, "the message");

  const call = discord.calls[0]!;
  assert.equal(call.channel, "555");
  assert.equal(call.auth, "Bot test-token");
  // Missing or malformed, this header stops you at Cloudflare — with an error that hides the cause.
  assert.match(call.agent ?? "", /^DiscordBot \(.+\)$/);
});

test("what another bot says does not count — including the one it just sent itself", async () => {
  let asked = 0;
  const { adapter, gateway } = setup({
    respond: async () => {
      asked += 1;
      return "the answer";
    },
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket, { author: { id: "9", username: "joy", bot: true } });
  message(socket, { content: "a human said this" });

  await stopWhen(adapter, running, () => asked > 0, "asking Joy");

  assert.equal(asked, 1, "without blocking its own echo it ends up talking to itself");
});

test("in a channel it stays quiet unless called; once called it answers, and the marker comes off", async () => {
  const asked: string[] = [];
  const { adapter, gateway } = setup({
    respond: async (_conversation, text) => {
      asked.push(text);
      return "the answer";
    },
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  // Channel chatter, not calling it
  message(socket, { guild_id: "1", content: "nice weather today" });
  // Calling it
  message(socket, { guild_id: "1", content: "<@9> note this down", mentions: [{ id: "9" }] });

  await stopWhen(adapter, running, () => asked.length > 0, "asking Joy");

  assert.deepEqual(asked, ["note this down"], "unaddressed chatter must not reach Joy, and the @ marker must not reach the model");
});

test("someone outside the allowlist: Joy is not asked and nothing is said back", async () => {
  const logs: string[] = [];
  let asked = 0;
  const { adapter, gateway, discord } = setup({
    allow: ["@alice"],
    respond: async () => {
      asked += 1;
      return "the answer";
    },
    onLog: (text) => logs.push(text),
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket, { channel_id: "1", author: { id: "99", username: "mallory" } });
  message(socket, { channel_id: "2", author: { id: "7", username: "alice" } });

  await stopWhen(adapter, running, () => discord.sent.length > 0, "the answer");

  assert.equal(asked, 1);
  assert.ok(logs.some((line) => line.includes("outside the allowlist")));
  assert.equal(discord.calls[0]!.channel, "2", "only the allowlisted person gets an answer");
});

test("an allowlisted login name is case-insensitive", async () => {
  const { adapter, gateway, discord } = setup({
    allow: ["@Alice"],
    respond: async () => "the answer",
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket, { author: { id: "7", username: "alice" } });

  await stopWhen(adapter, running, () => discord.sent.length > 0, "the answer");

  assert.deepEqual(discord.sent, ["the answer"]);
});

test("an attachment and no text: say plainly that it cannot be read", async () => {
  let asked = 0;
  const { adapter, gateway, discord } = setup({
    respond: async () => {
      asked += 1;
      return "the answer";
    },
  });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket, { content: "" });

  await stopWhen(adapter, running, () => discord.sent.length > 0, "the reply");

  assert.equal(asked, 0, "with no text, do not go asking the model");
  assert.match(discord.sent[0]!, /cannot read this one/);
});

test("Joy fails: the reason is said out loud instead of staying quiet", async () => {
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

  await stopWhen(adapter, running, () => discord.sent.length > 0, "the reply");

  // The chat box offers the user no other clue, so the problem class must be stated.
  assert.match(discord.sent[0]!, /the model side is not connected/);
  assert.match(discord.sent[0]!, /joy config/);
});

test("the reply cannot go out (kicked from the channel, say): leave a trace, the gateway must not fall over", async () => {
  const logs: string[] = [];
  const { adapter, gateway, discord } = setup({
    respond: async () => "the answer",
    onLog: (text) => logs.push(text),
  });
  discord.script({ status: 403, body: { message: "Missing Permissions" } });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket);

  // `await running` completing means the rejection was caught — uncaught, the whole process
  // would exit on one unhandled rejection.
  await stopWhen(
    adapter,
    running,
    () => logs.some((line) => line.includes("not handled")),
    "a trace",
  );

  assert.ok(
    logs.some((line) => line.includes("403")),
    "it has to say the send failed, not translate it into «cannot reach Joy» and point the wrong way",
  );
});

// ---- Sending out ----------------------------------------------------------------

test("an answer that is too long: split into several, each within the limit", async () => {
  const line =
    "this line is written long on purpose, so the total really exceeds one message and we can "
    + "see whether it gets cut hard.";
  const long = Array.from({ length: 120 }, (_, i) => `${i} ${line}`).join("\n");
  assert.ok(long.length > 2000);

  const { adapter, gateway, discord } = setup({ respond: async () => long });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket);

  await stopWhen(adapter, running, () => discord.sent.join("\n") === long, "the whole answer, sent in batches");

  assert.ok(
    discord.sent.every((chunk) => chunk.length <= 2000),
    "past the limit Discord just answers 400; it will not truncate for you",
  );
  assert.equal(discord.sent.join("\n"), long, "nothing may be lost in the split");
});

test("rate limited: wait as long as it says, then send once more", async () => {
  const { adapter, gateway, discord } = setup({ respond: async () => "the answer" });
  discord.script({ status: 429, body: { retry_after: 0.01 } });
  const running = adapter.run();
  const socket = gateway.latest;
  hello(socket);
  ready(socket);
  message(socket);

  await stopWhen(adapter, running, () => discord.sent.length > 0, "the resend");

  assert.equal(discord.calls.length, 2, "a 429 first, success the second time");
  assert.deepEqual(discord.sent, ["the answer"]);
});

test("the connection drops: it rests, reconnects itself, and starts with a fresh handshake", async () => {
  const logs: string[] = [];
  const { adapter, gateway } = setup({ onLog: (text) => logs.push(text) });
  const running = adapter.run();
  const first = gateway.latest;
  hello(first);
  ready(first);

  // The server took it down on purpose — Discord does this periodically; it's not a fault.
  first.close();

  const deadline = Date.now() + 5000;
  while (gateway.sockets.length < 2) {
    if (Date.now() > deadline) throw new Error("the reconnect never came");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }

  const second = gateway.latest;
  hello(second);
  // A fresh handshake, not RESUME — messages spoken during the drop are just lost. That's the
  // cost of not implementing RESUME, pinned here so it isn't chased as a bug for hours later.
  assert.equal(second.frames(2).length, 1, "a reconnect opens with IDENTIFY");

  adapter.stop();
  await running;

  assert.ok(
    logs.some((line) => line.includes("the connection dropped")),
    "a drop has to leave a trace, or a problem can only be guessed at",
  );
});
