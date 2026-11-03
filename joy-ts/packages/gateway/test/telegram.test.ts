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
      throw new Error("网络断了");
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
      from: { first_name: "某人", ...from },
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
      from: { id: chatId, first_name: "某人" },
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
    options.respond ?? (async () => "答复"),
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
    if (Date.now() > deadline) throw new Error("等到超时了");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  adapter.stop();
  await running;
}

// ---- Splitting long messages ---------------------------------------------------------------

test("短消息原样一条", () => {
  assert.deepEqual(splitMessage("你好", MESSAGE_LIMIT), ["你好"]);
});

test("超长的按换行切，一个字都不丢", () => {
  const text = Array.from({ length: 300 }, (_, index) => `第 ${index} 行`).join("\n");
  const chunks = splitMessage(text, 100);

  assert.ok(chunks.length > 1, "这么长本来就该切成多条");
  assert.ok(
    chunks.every((chunk) => chunk.length <= 100),
    "每一段都不能超上限",
  );
  assert.equal(chunks.join("\n"), text, "切完拼回去必须跟原文一样");
});

test("连一个换行都没有的超长内容也得切完", () => {
  const text = "x".repeat(250);
  const chunks = splitMessage(text, 100);
  assert.deepEqual(
    chunks.map((chunk) => chunk.length),
    [100, 100, 50],
  );
  assert.equal(chunks.join(""), text);
});

// ---- Adapter ----------------------------------------------------------------

test("来一条文字：按对话去问 Joy，把答复发回那个聊天", async () => {
  const asked: Array<[string, string]> = [];
  const { adapter, api } = setup({
    respond: async (conversation, text) => {
      asked.push([conversation, text]);
      return "你也好";
    },
  });
  api.pushUpdates([update(1, 42, "你好")]);

  await runUntil(adapter, () => api.sent.length > 0);

  // Sessions split by **chat**, not by person — a group shares one conversation or nobody can keep up.
  assert.deepEqual(asked, [["telegram:42", "你好"]]);
  assert.deepEqual(api.sent, ["你也好"]);
});

test("offset 推到已处理的最大 id 之后，同一条不会再来一遍", async () => {
  const { adapter, api } = setup();
  api.pushUpdates([update(7, 42, "一")]);
  api.pushUpdates([update(9, 42, "二")]);

  await runUntil(adapter, () => api.sent.length >= 2);

  assert.deepEqual(
    api.offsets.slice(0, 3),
    [0, 8, 10],
    "处理完 7 之后要从 8 开始要，处理完 9 之后从 10",
  );
});

test("不在白名单里的人：不问 Joy，也不回一句", async () => {
  const logs: string[] = [];
  let asked = 0;
  const { adapter, api } = setup({
    allow: ["@alice"],
    respond: async () => {
      asked += 1;
      return "答复";
    },
    onLog: (text) => logs.push(text),
  });
  // A stranger (id 99) speaks first, then the allowlisted alice.
  api.pushUpdates([update(1, 99, "你好")]);
  api.pushUpdates([update(2, 42, "我才是 alice", { id: 7, username: "alice" })]);

  await runUntil(adapter, () => api.sent.length > 0);

  assert.equal(asked, 1, "只有被允许的那条该去问 Joy");
  assert.deepEqual(api.sent, ["答复"]);
  assert.ok(
    logs.some((line) => line.includes("不在白名单")),
    "被拦下来的要留个痕",
  );
});

test("白名单里的用户名大小写不敏感", async () => {
  const { adapter, api } = setup({
    allow: ["@Alice"],
    respond: async () => "答复",
  });
  // The list says `@Alice`, the arrival is lowercase — usernames must be case-insensitive.
  api.pushUpdates([update(1, 42, "你好", { id: 7, username: "alice" })]);

  await runUntil(adapter, () => api.sent.length > 0);
  assert.deepEqual(api.sent, ["答复"]);
});

test("语音、图片这些读不了的，明说读不了", async () => {
  let asked = 0;
  const { adapter, api } = setup({
    respond: async () => {
      asked += 1;
      return "答复";
    },
  });
  api.pushUpdates([nonTextUpdate(1, 42)]);

  await runUntil(adapter, () => api.sent.length > 0);

  assert.equal(asked, 0);
  assert.match(api.sent[0]!, /读不了/);
});

test("Joy 那边出错：把原因说出来，而不是默不作声", async () => {
  const { adapter, api } = setup({
    respond: async () => {
      throw new JoyError({
        code: CODES.PROVIDER_ERROR,
        message: "401 Unauthorized",
      });
    },
  });
  api.pushUpdates([update(1, 42, "你好")]);

  await runUntil(adapter, () => api.sent.length > 0);

  assert.match(api.sent[0]!, /模型那边没接上/);
  assert.match(api.sent[0]!, /joy config/);
});

test("回答太长：分成几条发，不撞 Telegram 的上限", async () => {
  const long = Array.from(
    { length: 900 },
    (_, index) => `第 ${index} 行：这一行特意写长一点，好让总长度确实超过一条消息的上限。`,
  ).join("\n");
  // The expected chunk count is deterministic (splitMessage is pure) — assert only when **all**
  // chunks have arrived, else the async send mid-way yields half a list (Linux scheduling exposed
  // this race faster).
  const expected = splitMessage(long, MESSAGE_LIMIT);
  const { adapter, api } = setup({ respond: async () => long });
  api.pushUpdates([update(1, 42, "讲个长的")]);

  await runUntil(adapter, () => api.sent.length >= expected.length);

  assert.ok(api.sent.length > 1, "该被切成多条");
  assert.ok(api.sent.every((chunk) => chunk.length <= 4096));
  assert.equal(api.sent.join("\n"), long);
});

test("出网抖了一下：退避重试，进程不散", async () => {
  const logs: string[] = [];
  const { adapter, api } = setup({ onLog: (text) => logs.push(text) });
  api.breakOnce();
  api.pushUpdates([update(1, 42, "重试之后才收到的")]);

  await runUntil(adapter, () => api.sent.length > 0);

  assert.deepEqual(api.sent, ["答复"]);
  assert.ok(
    logs.some((line) => line.includes("长轮询失败")),
    "重试这件事要说一声，不然看起来就是卡住了",
  );
});

test("回话被拒（比如用户把 bot 封了）：只留个痕，网关不能跟着倒", async () => {
  const logs: string[] = [];
  const { adapter, api } = setup({ onLog: (text) => logs.push(text) });
  api.refuseSendingOnce();
  api.pushUpdates([update(1, 42, "你好")]);

  // `runUntil` awaits run() at the end: no rejection means the error was caught — uncaught, the
  // whole process would exit on one unhandled rejection.
  await runUntil(adapter, () => logs.some((line) => line.includes("没处理完")));

  assert.ok(
    logs.some((line) => line.includes("Forbidden")),
    "得说清是发不出去，而不是翻成「连不上 Joy」指错方向",
  );
});
