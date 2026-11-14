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
    assert.ok(line !== undefined, "nothing has been written out yet");
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

test("requests carry an increasing id, and the response goes back to the await that asked", async () => {
  const { client, transport } = setup();

  const first = client.request(METHODS.TURN_START, { message: "hello" });
  const second = client.request(METHODS.SESSION_LIST, {});

  assert.deepEqual(
    transport.written.map((line) => JSON.parse(line).method),
    ["turn/start", "session/list"],
  );
  const [id1, id2] = transport.written.map((line) => JSON.parse(line).id);
  assert.notEqual(id1, id2, "two requests must not share an id");

  // Deliberately out of order: pairing by id is the whole point.
  transport.respond(id2, { data: [], nextCursor: null });
  transport.respond(id1, { turnId: "t-1" });

  assert.deepEqual(await second, { data: [], nextCursor: null });
  assert.deepEqual(await first, { turnId: "t-1" });
});

test("an error frame becomes a JoyError, branched on by code", async () => {
  const { client, transport } = setup();

  const denied = client.request(METHODS.TURN_START, { message: "hi" });
  transport.fail(transport.lastRequest.id, CODES.PROVIDER_ERROR, "no usable key");

  const error = await denied.then(
    () => assert.fail("this should have been rejected"),
    (caught: unknown) => caught,
  );
  assert.ok(error instanceof JoyError);
  assert.equal(error.message, "no usable key");
  assert.ok(error.isProvider, "PROVIDER_ERROR has to be judged as the provider kind");
  assert.ok(!error.isNotImplemented);

  const todo = client.request(METHODS.MODEL_LIST, {});
  transport.fail(transport.lastRequest.id, CODES.NOT_IMPLEMENTED, "not built yet");
  await assert.rejects(todo, (caught: unknown) => {
    assert.ok(caught instanceof JoyError);
    assert.ok(caught.isNotImplemented);
    return true;
  });
});

test("notifications fan out on the discriminant; after unsubscribing nothing arrives", async () => {
  const { client, transport } = setup();
  const seen: string[] = [];
  const unsubscribe = client.onNotification((notification) =>
    seen.push(notification.type),
  );

  transport.notify({
    type: "turnStarted",
    turnId: "t-1",
    sessionId: "s-1",
    userMessage: "hello",
    ts: "2026-09-19T00:00:00.000Z",
  });
  transport.notify({ type: "textDelta", turnId: "t-1", delta: "he" });

  assert.deepEqual(seen, ["turnStarted", "textDelta"]);

  unsubscribe();
  transport.notify({
    type: "error",
    code: CODES.INTERNAL_ERROR,
    message: "this must not be seen after unsubscribing",
    data: null,
  });
  assert.deepEqual(seen, ["turnStarted", "textDelta"], "nothing may arrive after unsubscribing");
});

test("anything unreadable goes to the log instead of crashing the client", async () => {
  const { transport, logs } = setup();

  transport.emitRaw("this is not JSON");
  transport.emitRaw(JSON.stringify({ jsonrpc: "2.0", method: "other/method" }));
  transport.emitRaw(
    JSON.stringify({ jsonrpc: "2.0", method: "turn/notification" }),
  );
  transport.emitRaw(JSON.stringify({ jsonrpc: "2.0", id: 999, result: {} }));
  transport.emitRaw(
    JSON.stringify({
      jsonrpc: "2.0",
      id: null,
      error: { code: CODES.PARSE_ERROR, message: "what you sent is not JSON" },
    }),
  );
  transport.emitLog("the MCP server fs cannot be reached; skipping");

  assert.equal(logs.length, 6, `there should be 6 log lines, actually: ${JSON.stringify(logs)}`);
  assert.match(logs[0]!, /cannot be read/);
  assert.match(logs[1]!, /unknown notification method/);
  assert.match(logs[2]!, /carries nothing/);
  assert.match(logs[3]!, /nobody is waiting/);
  assert.match(logs[4]!, /is not JSON/);
  assert.match(logs[5]!, /fs cannot be reached/);
});

test("on a dropped connection every pending request is rejected, and it says what it waited for", async () => {
  const { client, transport } = setup();

  const waiting = client.request(METHODS.MEMORY_SEARCH, { query: "coffee" });
  transport.emitClose("exited (code=1 signal=null)");

  await assert.rejects(waiting, /memory\/search never got its response; the connection dropped/);

  // After the drop, new requests are rejected at once — they must not hang waiting for a reply that will never come.
  await assert.rejects(
    client.request(METHODS.SESSION_NEW, {}),
    /the connection is gone/,
  );
});

test("a deliberate close wraps up politely and settles the pending requests", async () => {
  const { client, transport } = setup();

  const waiting = client.request(METHODS.CONFIG_READ, {});
  client.close();

  assert.ok(transport.closeCalled, "it has to close the transport (that is closing stdin, not killing)");
  await assert.rejects(waiting, /config\/read never got its response/);
});

test("method names come from the artefacts, letter for letter with the Rust side", () => {
  // These assertions "pin the artifacts to the ground": the day Rust renames a method, the
  // artifacts change and this goes red — instead of a runtime "method not found" much later.
  assert.equal(METHODS.TURN_START, "turn/start");
  assert.equal(METHODS.SESSION_NEW, "session/new");
  assert.equal(METHODS.MEMORY_REMEMBER, "memory/remember");
  assert.equal(CODES.NOT_IMPLEMENTED, -32002);
});
