import assert from "node:assert/strict";
import test from "node:test";

import { CODES, JoyClient, JoyError } from "@joy/client";
import type { ServerNotification, Transport } from "@joy/client";

import { JoyBridge, explain, sessionIdFor } from "../src/bridge.ts";
import type { ProgressNote } from "../src/bridge.ts";

interface SentRequest {
  id: number;
  method: string;
  params: Record<string, unknown>;
}

/** 站在服务端那一侧的假传输：收请求、吐通知、给应答。 */
class FakeServer implements Transport {
  readonly written: SentRequest[] = [];
  closeCalled = false;

  #onLine?: (line: string) => void;

  write(line: string): void {
    this.written.push(JSON.parse(line) as SentRequest);
  }

  onLine(handler: (line: string) => void): void {
    this.#onLine = handler;
  }

  onLog(): void {}
  onClose(): void {}

  close(): void {
    this.closeCalled = true;
  }

  /** 给第 `index` 条请求发回应答。 */
  reply(index: number, result: unknown): void {
    const request = this.written[index];
    assert.ok(request !== undefined, `第 ${index} 条请求还没写出去`);
    this.#onLine?.(JSON.stringify({ jsonrpc: "2.0", id: request.id, result }));
  }

  /** 给第 `index` 条请求回错误。 */
  fail(index: number, code: number, message: string): void {
    const request = this.written[index];
    assert.ok(request !== undefined, `第 ${index} 条请求还没写出去`);
    this.#onLine?.(
      JSON.stringify({ jsonrpc: "2.0", id: request.id, error: { code, message } }),
    );
  }

  notify(notification: ServerNotification): void {
    this.#onLine?.(
      JSON.stringify({
        jsonrpc: "2.0",
        method: "turn/notification",
        params: notification,
      }),
    );
  }
}

function setup(options: { onProgress?: (note: ProgressNote) => void } = {}): {
  bridge: JoyBridge;
  server: FakeServer;
} {
  const server = new FakeServer();
  const client = new JoyClient({ transport: server, onLog: () => {} });
  const bridge = new JoyBridge({ client, onProgress: options.onProgress });
  return { bridge, server };
}

function meta(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    gate: null,
    graph: null,
    iterations: 1,
    latencyMs: 12,
    tools: [],
    model: "small-model",
    provider: "deepseek",
    ...overrides,
  };
}

function started(turnId: string, sessionId = "telegram:42"): ServerNotification {
  return {
    type: "turnStarted",
    turnId,
    sessionId,
    userMessage: "你好",
    ts: "2026-09-19T00:00:00.000Z",
  };
}

function completed(
  turnId: string,
  reply: string,
  metaOverrides: Record<string, unknown> = {},
): ServerNotification {
  return {
    type: "turnCompleted",
    turnId,
    reply,
    iterations: 1,
    meta: meta(metaOverrides) as never,
    usage: null,
  };
}

/** 让排着的 microtask 跑完 —— 用来观察「第二条请求出去了没有」。 */
function tick(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

test("会话 id 由平台身份派生，网关因此不需要记任何映射", () => {
  assert.equal(sessionIdFor("telegram:42"), "telegram:42");
  // 同一个人下次连上来还是同一段对话，重启也不变 —— 因为压根没存过什么。
  assert.equal(sessionIdFor("telegram:42"), "telegram:42");
});

test("一轮：请求写对了，答复和遥测原样带回来", async () => {
  const { bridge, server } = setup();

  const promise = bridge.ask("telegram:42", "你好");
  await tick();
  assert.equal(server.written.length, 1);

  const request = server.written[0]!;
  assert.equal(request.method, "turn/start");
  assert.equal(request.params["sessionId"], "telegram:42");
  assert.equal(request.params["message"], "你好");
  assert.equal(
    request.params["stream"],
    false,
    "聊天平台要的是一整句，不是逐字抖动",
  );

  server.notify(started("t1"));
  server.notify({ type: "toolStarted", turnId: "t1", tool: "memory_search", args: {} });
  server.notify({
    type: "toolCompleted",
    turnId: "t1",
    tool: "memory_search",
    output: "三条",
    status: "ok",
    durationMs: 3,
  });
  server.notify(completed("t1", "你好呀", { model: "big-model", iterations: 2 }));
  server.reply(0, { turnId: "t1" });

  const outcome = await promise;
  assert.equal(outcome.sessionId, "telegram:42");
  assert.equal(outcome.reply, "你好呀");
  assert.deepEqual(outcome.tools, ["memory_search"]);
  assert.equal(outcome.model, "big-model");
  assert.equal(outcome.iterations, 2);
});

test("同一段对话连发两条：第二条必须等第一条走完", async () => {
  const { bridge, server } = setup();

  const first = bridge.ask("telegram:42", "第一条");
  const second = bridge.ask("telegram:42", "第二条");

  await tick();
  assert.equal(server.written.length, 1, "两条消息不能同时占着服务端");

  server.notify(started("t1"));
  server.notify(completed("t1", "第一答"));
  server.reply(0, { turnId: "t1" });
  assert.equal((await first).reply, "第一答");

  await tick();
  assert.equal(server.written.length, 2, "第一条回来了，第二条才该出去");
  assert.equal(server.written[1]!.params["message"], "第二条");

  server.notify(started("t2"));
  server.notify(completed("t2", "第二答"));
  server.reply(1, { turnId: "t2" });
  assert.equal((await second).reply, "第二答");
});

test("不同对话之间互不影响", async () => {
  const { bridge, server } = setup();

  // 两个人同时说话：排队是按对话分的，不该互相卡住。
  const alice = bridge.ask("telegram:1", "我是甲");
  const bob = bridge.ask("telegram:2", "我是乙");
  await tick();
  assert.equal(server.written.length, 2, "不同对话不该互相排队");

  assert.equal(server.written[0]!.params["sessionId"], "telegram:1");
  assert.equal(server.written[1]!.params["sessionId"], "telegram:2");

  server.notify(started("t1", "telegram:1"));
  server.notify(completed("t1", "甲好"));
  server.reply(0, { turnId: "t1" });
  server.notify(started("t2", "telegram:2"));
  server.notify(completed("t2", "乙好"));
  server.reply(1, { turnId: "t2" });

  assert.equal((await alice).reply, "甲好");
  assert.equal((await bob).reply, "乙好");
});

test("一轮失败：错误穿透出去，残骸不留到下一轮", async () => {
  const { bridge, server } = setup();

  const first = bridge.ask("telegram:42", "这句会失败");
  await tick();
  server.notify(started("t1"));
  server.notify(completed("t1", "第一轮的答复"));
  // 通知串走完了，应答却是错的 —— 收着的那份必须扔掉。
  server.fail(0, CODES.INTERNAL_ERROR, "写库失败");
  await assert.rejects(first, /写库失败/);

  const second = bridge.ask("telegram:42", "这句会成功");
  await tick();
  server.notify(started("t2"));
  server.notify(completed("t2", "第二轮的答复"));
  server.reply(1, { turnId: "t2" });

  assert.equal((await second).reply, "第二轮的答复", "不能捡到上一轮的答复");
});

test("应答来了通知串却断了：明确报错，不糊一个空答复过去", async () => {
  const { bridge, server } = setup();

  const promise = bridge.ask("telegram:42", "你好");
  await tick();
  server.notify(started("t1"));
  server.reply(0, { turnId: "t1" });

  await assert.rejects(promise, /没有收到 turnCompleted/);
});

test("进度：工具开跑说一声，工具失败也值得说一声", async () => {
  const notes: string[] = [];
  const { bridge, server } = setup({
    onProgress: (note) => notes.push(note.text),
  });

  const promise = bridge.ask("telegram:42", "帮我查查");
  await tick();
  server.notify(started("t1"));
  server.notify({ type: "toolStarted", turnId: "t1", tool: "read_file", args: {} });
  server.notify({
    type: "toolCompleted",
    turnId: "t1",
    tool: "read_file",
    output: "ENOENT",
    status: "error",
    durationMs: 1,
  });
  // 工具成功是常态，不该打扰用户；失败才报 —— 下一句解释通常就跟它有关。
  server.notify({ type: "toolStarted", turnId: "t1", tool: "http_get", args: {} });
  server.notify({
    type: "toolCompleted",
    turnId: "t1",
    tool: "http_get",
    output: "200",
    status: "ok",
    durationMs: 20,
  });
  // 这条不带 turnId，归不到任何一段对话 —— 该被无声丢掉，不能猜着播报。
  server.notify({ type: "consolidationCompleted", newFacts: 2 });
  server.notify(completed("t1", "查到了"));
  server.reply(0, { turnId: "t1" });

  await promise;
  assert.deepEqual(notes, [
    "在用 read_file…",
    "read_file 没成功，它换条路继续",
    "在用 http_get…",
  ]);
});

test("错误翻成人话：模型没配好、还没做、连不上，是三件事", () => {
  const provider = new JoyError({
    code: CODES.PROVIDER_ERROR,
    message: "401 Unauthorized",
  });
  assert.match(explain(provider), /joy config/, "得告诉用户去哪儿解决");

  const todo = new JoyError({
    code: CODES.NOT_IMPLEMENTED,
    message: "'model/list' 还没实现",
  });
  assert.match(explain(todo), /还没做/);

  assert.match(explain(new Error("spawn joy ENOENT")), /连不上 Joy/);
  assert.match(explain("随便什么东西"), /连不上 Joy/);
});

test("关掉网关就把 app-server 的 stdin 关上", () => {
  const { bridge, server } = setup();
  bridge.close();
  assert.ok(server.closeCalled);
});
