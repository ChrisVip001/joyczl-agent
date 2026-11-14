import assert from "node:assert/strict";
import test from "node:test";

import { CODES, JoyError } from "@joy/client";

import { MESSAGE_LIMIT, TelegramAdapter, splitMessage } from "../src/telegram.ts";

interface Call {
  method: string;
  body: Record<string, unknown>;
}

/** A fake Telegram API. The requests that go out have the same shape as real ones. */
class FakeTelegram {
  readonly calls: Call[] = [];
  readonly sent: string[] = [];

  /** Queued `getUpdates` results, served in order; empty forever once exhausted. */
  readonly #batches: unknown[][] = [];
  #failNext = 0;
  #refuseSending = 0;

  pushUpdates(updates: unknown[]): void {
    this.#batches.push(updates);
  }

  /** Fail the next request (simulating a network drop / 502). */
  breakOnce(): void {
    this.#failNext += 1;
  }

  /** The next "reply" is refused — the user blocked the bot or it was kicked, as on a real phone. */
  refuseSendingOnce(): void {
    this.#refuseSending += 1;
  }

  readonly fetch = async (
    input: string | URL | Request,
    init?: RequestInit,
  ): Promise<Response> => {
    // Yield the event loop before answering.
    //
    // Not decoration: the adapter's long poll is `while (!stopped) await fetch(...)`, and a fake
    // fetch that resolves **immediately** compresses the whole loop into a string of microtasks —
    // timers never get a turn before the microtask queue drains, so `stop()` is never delivered.
    // A real network always waits a round trip and always yields; matching that makes a faithful double.
    await new Promise((resolve) => setImmediate(resolve));

    if (this.#failNext > 0) {
      this.#failNext -= 1;
      throw new Error("the network dropped");
    }

    const method = String(input).split("/").pop() ?? "";
    const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
    this.calls.push({ method, body });

    if (method === "getUpdates") {
      return respond({ ok: true, result: this.#batches.shift() ?? [] });
    }
    if (this.#refuseSending > 0) {
      this.#refuseSending -= 1;
      return respond({ ok: false, description: "Forbidden: bot was blocked by the user" });
    }
    this.sent.push(String(body["text"]));
    return respond({ ok: true, result: { message_id: this.sent.length } });
  };

  get offsets(): unknown[] {
    return this.calls
      .filter((call) => call.method === "getUpdates")
      .map((call) => call.body["offset"]);
  }
}

function respond(value: unknown): Response {
  return {
    status: 200,
    json: async () => value,
  } as unknown as Response;
}

function update(
  updateId: number,
  chatId: number,
  text: string,
  from: { id: number; username?: string } = { id: chatId },
): unknown {
  return {
    update_id: updateId,
    message: {
      message_id: updateId,
      chat: { id: chatId, type: "private" },
      from: { first_name: "someone", ...from },
      text,
    },
  };
}

/** The kind of message without text: voice, images, and stickers all look like this. */
function nonTextUpdate(updateId: number, chatId: number): unknown {
  return {
    update_id: updateId,
    message: {
      message_id: updateId,
      chat: { id: chatId, type: "private" },
      from: { id: chatId, first_name: "someone" },
      voice: { file_id: "abc", duration: 3 },
    },
  };
}

function setup(
  options: {
    allow?: readonly string[];
    respond?: (conversation: string, text: string) => Promise<string>;
    onLog?: (text: string) => void;
  } = {},
): { adapter: TelegramAdapter; api: FakeTelegram } {
  const api = new FakeTelegram();
  const adapter = new TelegramAdapter(
    {
      token: "test-token",
      allow: options.allow,
      pollSeconds: 0,
      fetch: api.fetch,
      onLog: options.onLog ?? (() => {}),
    },
    options.respond ?? (async () => "the answer"),
  );
  return { adapter, api };
}

/** Loop until `done()` holds, then stop the poll and await its real shutdown. */
async function runUntil(
  adapter: TelegramAdapter,
  done: () => boolean,
): Promise<void> {
  const running = adapter.run();
  const deadline = Date.now() + 5000;
  while (!done()) {
    if (Date.now() > deadline) throw new Error("timed out");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  adapter.stop();
  await running;
}

// ---- Splitting long messages ---------------------------------------------------------------

test("a short message stays one", () => {
  assert.deepEqual(splitMessage("hello", MESSAGE_LIMIT), ["hello"]);
});

test("an overlong one is cut at newlines, losing not a character", () => {
  const text = Array.from({ length: 300 }, (_, index) => `line ${index}`).join("\n");
  const chunks = splitMessage(text, 100);

  assert.ok(chunks.length > 1, "something this long has to be split");
  assert.ok(
    chunks.every((chunk) => chunk.length <= 100),
    "no chunk may pass the limit",
  );
  assert.equal(chunks.join("\n"), text, "joined back together it has to match the original");
});

test("an overlong blob with no newline at all still gets split", () => {
  const text = "x".repeat(250);
  const chunks = splitMessage(text, 100);
  assert.deepEqual(
    chunks.map((chunk) => chunk.length),
    [100, 100, 50],
  );
  assert.equal(chunks.join(""), text);
});

// ---- Adapter ----------------------------------------------------------------

test("a message arrives: ask Joy by chat, and send the answer back to that chat", async () => {
  const asked: Array<[string, string]> = [];
  const { adapter, api } = setup({
    respond: async (conversation, text) => {
      asked.push([conversation, text]);
      return "hello to you too";
    },
  });
  api.pushUpdates([update(1, 42, "hello")]);

  await runUntil(adapter, () => api.sent.length > 0);

  // Sessions split by **chat**, not by person — a group shares one conversation or nobody can keep up.
  assert.deepEqual(asked, [["telegram:42", "hello"]]);
  assert.deepEqual(api.sent, ["hello to you too"]);
});

test("the offset moves past the biggest handled id, so nothing is seen twice", async () => {
  const { adapter, api } = setup();
  api.pushUpdates([update(7, 42, "one")]);
  api.pushUpdates([update(9, 42, "two")]);

  await runUntil(adapter, () => api.sent.length >= 2);

  assert.deepEqual(
    api.offsets.slice(0, 3),
    [0, 8, 10],
    "after 7 it asks from 8, after 9 from 10",
  );
});

test("someone outside the allowlist: Joy is not asked and nothing is said back", async () => {
  const logs: string[] = [];
  let asked = 0;
  const { adapter, api } = setup({
    allow: ["@alice"],
    respond: async () => {
      asked += 1;
      return "the answer";
    },
    onLog: (text) => logs.push(text),
  });
  // A stranger (id 99) speaks first, then the allowlisted alice.
  api.pushUpdates([update(1, 99, "hello")]);
  api.pushUpdates([update(2, 42, "I am the real alice", { id: 7, username: "alice" })]);

  await runUntil(adapter, () => api.sent.length > 0);

  assert.equal(asked, 1, "only the allowed one goes to Joy");
  assert.deepEqual(api.sent, ["the answer"]);
  assert.ok(
    logs.some((line) => line.includes("outside the allowlist")),
    "what was blocked has to leave a trace",
  );
});

test("an allowlisted username is case-insensitive", async () => {
  const { adapter, api } = setup({
    allow: ["@Alice"],
    respond: async () => "the answer",
  });
  // The list says `@Alice`, the arrival is lowercase — usernames must be case-insensitive.
  api.pushUpdates([update(1, 42, "hello", { id: 7, username: "alice" })]);

  await runUntil(adapter, () => api.sent.length > 0);
  assert.deepEqual(api.sent, ["the answer"]);
});

test("voice and images cannot be read, and it says so plainly", async () => {
  let asked = 0;
  const { adapter, api } = setup({
    respond: async () => {
      asked += 1;
      return "the answer";
    },
  });
  api.pushUpdates([nonTextUpdate(1, 42)]);

  await runUntil(adapter, () => api.sent.length > 0);

  assert.equal(asked, 0);
  assert.match(api.sent[0]!, /cannot read this one/);
});

test("Joy fails: the reason is said out loud instead of staying quiet", async () => {
  const { adapter, api } = setup({
    respond: async () => {
      throw new JoyError({
        code: CODES.PROVIDER_ERROR,
        message: "401 Unauthorized",
      });
    },
  });
  api.pushUpdates([update(1, 42, "hello")]);

  await runUntil(adapter, () => api.sent.length > 0);

  assert.match(api.sent[0]!, /the model side is not connected/);
  assert.match(api.sent[0]!, /joy config/);
});

test("an answer that is too long goes out in several, never past Telegram's limit", async () => {
  const long = Array.from(
    { length: 900 },
    (_, index) => `line ${index}: this line is written long on purpose so the total really exceeds one message.`,
  ).join("\n");
  // The expected chunk count is deterministic (splitMessage is pure) — assert only when **all**
  // chunks have arrived, else the async send mid-way yields half a list (Linux scheduling exposed
  // this race faster).
  const expected = splitMessage(long, MESSAGE_LIMIT);
  const { adapter, api } = setup({ respond: async () => long });
  api.pushUpdates([update(1, 42, "tell me something long")]);

  await runUntil(adapter, () => api.sent.length >= expected.length);

  assert.ok(api.sent.length > 1, "it has to be split");
  assert.ok(api.sent.every((chunk) => chunk.length <= 4096));
  assert.equal(api.sent.join("\n"), long);
});

test("the network hiccups: back off and retry, the process holds together", async () => {
  const logs: string[] = [];
  const { adapter, api } = setup({ onLog: (text) => logs.push(text) });
  api.breakOnce();
  api.pushUpdates([update(1, 42, "this one only arrives after the retry")]);

  await runUntil(adapter, () => api.sent.length > 0);

  assert.deepEqual(api.sent, ["the answer"]);
  assert.ok(
    logs.some((line) => line.includes("long polling failed")),
    "a retry has to be announced, or it just looks stuck",
  );
});

test("the reply is refused (the user blocked the bot, say): leave a trace, the gateway must not fall over", async () => {
  const logs: string[] = [];
  const { adapter, api } = setup({ onLog: (text) => logs.push(text) });
  api.refuseSendingOnce();
  api.pushUpdates([update(1, 42, "hello")]);

  // `runUntil` awaits run() at the end: no rejection means the error was caught — uncaught, the
  // whole process would exit on one unhandled rejection.
  await runUntil(adapter, () => logs.some((line) => line.includes("not handled")));

  assert.ok(
    logs.some((line) => line.includes("Forbidden")),
    "it has to say the send failed, not translate it into «cannot reach Joy» and point the wrong way",
  );
});
