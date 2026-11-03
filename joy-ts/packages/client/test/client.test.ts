import assert from "node:assert/strict";
import test from "node:test";

import { JoyClient, JoyError } from "../src/client.ts";
import { CODES, METHODS } from "../src/protocol.ts";
import type { ServerNotification } from "../src/protocol.ts";
import type { Transport } from "../src/transport.ts";

/**
 * A fake transport standing on the server's side.
 *
 * Everything tested here is the client's own logic (id pairing, dispatch, shutdown settling) —
 * not one line needs a real process; that belongs to the smoke tests, where it belongs.
 */
class FakeTransport implements Transport {
  readonly written: string[] = [];
  closeCalled = false;

  #onLine?: (line: string) => void;
  #onLog?: (text: string) => void;
  #onClose?: (reason: string) => void;

  write(line: string): void {
    this.written.push(line);
  }

  onLine(handler: (line: string) => void): void {
    this.#onLine = handler;
  }

  onLog(handler: (text: string) => void): void {
    this.#onLog = handler;
  }

  onClose(handler: (reason: string) => void): void {
    this.#onClose = handler;
  }

  close(): void {
    this.closeCalled = true;
  }

  /** The latest request, parsed. */
  get lastRequest(): { id: number; method: string; params: unknown } {
    const line = this.written.at(-1);
    assert.ok(line !== undefined, "还没有写出任何请求");
    return JSON.parse(line) as { id: number; method: string; params: unknown };
  }

  respond(id: number, result: unknown): void {
    this.#onLine?.(JSON.stringify({ jsonrpc: "2.0", id, result }));
  }

  fail(id: number, code: number, message: string): void {
    this.#onLine?.(
      JSON.stringify({ jsonrpc: "2.0", id, error: { code, message } }),
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

  emitRaw(line: string): void {
    this.#onLine?.(line);
  }

  emitLog(text: string): void {
    this.#onLog?.(text);
  }

  emitClose(reason: string): void {
    this.#onClose?.(reason);
  }
}

/** Build a client, collecting logs into an array for assertions. */
function setup(): { client: JoyClient; transport: FakeTransport; logs: string[] } {
  const transport = new FakeTransport();
  const logs: string[] = [];
  const client = new JoyClient({ transport, onLog: (text) => logs.push(text) });
  return { client, transport, logs };
}

test("请求带上递增的 id，应答还给对应的那个 await", async () => {
  const { client, transport } = setup();

  const first = client.request(METHODS.TURN_START, { message: "你好" });
  const second = client.request(METHODS.SESSION_LIST, {});

  assert.deepEqual(
    transport.written.map((line) => JSON.parse(line).method),
    ["turn/start", "session/list"],
  );
  const [id1, id2] = transport.written.map((line) => JSON.parse(line).id);
  assert.notEqual(id1, id2, "两个请求不能撞 id");

  // Deliberately out of order: pairing by id is the whole point.
  transport.respond(id2, { data: [], nextCursor: null });
  transport.respond(id1, { turnId: "t-1" });

  assert.deepEqual(await second, { data: [], nextCursor: null });
  assert.deepEqual(await first, { turnId: "t-1" });
});

test("错误帧还原成 JoyError，并按 code 分流", async () => {
  const { client, transport } = setup();

  const denied = client.request(METHODS.TURN_START, { message: "hi" });
  transport.fail(transport.lastRequest.id, CODES.PROVIDER_ERROR, "没有可用的 key");

  const error = await denied.then(
    () => assert.fail("本该被拒"),
    (caught: unknown) => caught,
  );
  assert.ok(error instanceof JoyError);
  assert.equal(error.message, "没有可用的 key");
  assert.ok(error.isProvider, "PROVIDER_ERROR 该被判成 provider 那类");
  assert.ok(!error.isNotImplemented);

  const todo = client.request(METHODS.MODEL_LIST, {});
  transport.fail(transport.lastRequest.id, CODES.NOT_IMPLEMENTED, "还没做");
  await assert.rejects(todo, (caught: unknown) => {
    assert.ok(caught instanceof JoyError);
    assert.ok(caught.isNotImplemented);
    return true;
  });
});

test("通知按判别式分发；退订之后不再收到", async () => {
  const { client, transport } = setup();
  const seen: string[] = [];
  const unsubscribe = client.onNotification((notification) =>
    seen.push(notification.type),
  );

  transport.notify({
    type: "turnStarted",
    turnId: "t-1",
    sessionId: "s-1",
    userMessage: "你好",
    ts: "2026-09-19T00:00:00.000Z",
  });
  transport.notify({ type: "textDelta", turnId: "t-1", delta: "你" });

  assert.deepEqual(seen, ["turnStarted", "textDelta"]);

  unsubscribe();
  transport.notify({
    type: "error",
    code: CODES.INTERNAL_ERROR,
    message: "退订后这个不该再被看见",
    data: null,
  });
  assert.deepEqual(seen, ["turnStarted", "textDelta"], "退订后不该再来");
});

test("读不懂的东西一律走日志，不让客户端崩", async () => {
  const { transport, logs } = setup();

  transport.emitRaw("这不是 JSON");
  transport.emitRaw(JSON.stringify({ jsonrpc: "2.0", method: "别的/方法" }));
  transport.emitRaw(
    JSON.stringify({ jsonrpc: "2.0", method: "turn/notification" }),
  );
  transport.emitRaw(JSON.stringify({ jsonrpc: "2.0", id: 999, result: {} }));
  transport.emitRaw(
    JSON.stringify({
      jsonrpc: "2.0",
      id: null,
      error: { code: CODES.PARSE_ERROR, message: "你发的不是 JSON" },
    }),
  );
  transport.emitLog("MCP 服务器 fs 连不上，跳过");

  assert.equal(logs.length, 6, `该有 6 条日志，实际：${JSON.stringify(logs)}`);
  assert.match(logs[0]!, /读不出来/);
  assert.match(logs[1]!, /不认识的通知方法/);
  assert.match(logs[2]!, /通知没有内容/);
  assert.match(logs[3]!, /id 没人在等/);
  assert.match(logs[4]!, /你发的不是 JSON/);
  assert.match(logs[5]!, /MCP 服务器 fs 连不上/);
});

test("连接断了：挂着的请求全被拒，且说得出在等什么", async () => {
  const { client, transport } = setup();

  const waiting = client.request(METHODS.MEMORY_SEARCH, { query: "咖啡" });
  transport.emitClose("退出（code=1 signal=null）");

  await assert.rejects(waiting, /memory\/search 没等到应答，连接断了/);

  // After the drop, new requests are rejected at once — they must not hang waiting for a reply that will never come.
  await assert.rejects(
    client.request(METHODS.SESSION_NEW, {}),
    /连接已经没了/,
  );
});

test("主动关闭：文明收尾，并收敛挂着的请求", async () => {
  const { client, transport } = setup();

  const waiting = client.request(METHODS.CONFIG_READ, {});
  client.close();

  assert.ok(transport.closeCalled, "该去关传输层（它是关 stdin，不是 kill）");
  await assert.rejects(waiting, /config\/read 没等到应答/);
});

test("方法名来自生成物，跟 Rust 侧一字不差", () => {
  // These assertions "pin the artifacts to the ground": the day Rust renames a method, the
  // artifacts change and this goes red — instead of a runtime "method not found" much later.
  assert.equal(METHODS.TURN_START, "turn/start");
  assert.equal(METHODS.SESSION_NEW, "session/new");
  assert.equal(METHODS.MEMORY_REMEMBER, "memory/remember");
  assert.equal(CODES.NOT_IMPLEMENTED, -32002);
});
