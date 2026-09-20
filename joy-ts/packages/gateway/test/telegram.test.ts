import assert from "node:assert/strict";
import test from "node:test";

import { CODES, JoyError } from "@joy/client";

import { MESSAGE_LIMIT, TelegramAdapter, splitMessage } from "../src/telegram.ts";

interface Call {
  method: string;
  body: Record<string, unknown>;
}

/** 假的 Telegram API。真发出去的那些请求，形状跟真的一样。 */
class FakeTelegram {
  readonly calls: Call[] = [];
  readonly sent: string[] = [];

  /** 排好队的 `getUpdates` 结果，按次发；发完就一直给空。 */
  readonly #batches: unknown[][] = [];
  #failNext = 0;
  #refuseSending = 0;

  pushUpdates(updates: unknown[]): void {
    this.#batches.push(updates);
  }

  /** 下一次请求失败（模拟断网 / 502）。 */
  breakOnce(): void {
    this.#failNext += 1;
  }

  /** 下一次「回话」被拒 —— 用户把 bot 封了、被踢出群，真机上就是这么回事。 */
  refuseSendingOnce(): void {
    this.#refuseSending += 1;
  }

  readonly fetch = async (
    input: string | URL | Request,
    init?: RequestInit,
  ): Promise<Response> => {
    // 先让出事件循环，再回答。
    //
    // 这不是装饰：适配器的长轮询是 `while (!stopped) await fetch(...)`，
    // 而一个**立刻** resolve 的假 fetch 会把整个循环压缩成一串 microtask ——
    // microtask 队列排空之前定时器根本没机会跑，`stop()` 也就永远递不进去。
    // 真网络每次都得等一个往返，一定会让出，所以这里照做才是忠实的替身。
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

/** 没有文字的那种消息：语音、图片、贴纸都长这样。 */
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

/** 一直转到 `done()` 成立，然后把轮询停掉、等它真的收尾。 */
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

// ---- 切长消息 ---------------------------------------------------------------

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

// ---- 适配器 ----------------------------------------------------------------

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

  // 会话按**聊天**分，不按人分 —— 群里大家共用一段对话才聊得下去。
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
  // 陌生人（id 99）先来搭话，然后才是名单里的 alice。
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
  // 名单里写的是 `@Alice`，来的是小写 —— 用户名不该分大小写。
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
  // 期望的切片数是确定的（splitMessage 是纯函数）——等到**全部**切片
  // 到齐再断言，否则异步发送中途截到的只是半张清单（Linux 调度更快
  // 暴露了这个竞态）。
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

  // `runUntil` 最后要 await 那个 run()：它没被拒绝，就说明这个错被兜住了
  // —— 没兜住的话，整个进程会因为一次未捕获的拒绝直接退出。
  await runUntil(adapter, () => logs.some((line) => line.includes("没处理完")));

  assert.ok(
    logs.some((line) => line.includes("Forbidden")),
    "得说清是发不出去，而不是翻成「连不上 Joy」指错方向",
  );
});
