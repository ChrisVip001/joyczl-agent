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

/** A fake transport on the server's side: take requests, emit notifications, return replies. */
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

  /** Reply to request number `index`. */
  reply(index: number, result: unknown): void {
    const request = this.written[index];
    assert.ok(request !== undefined, `request ${index} was never written out`);
    this.#onLine?.(JSON.stringify({ jsonrpc: "2.0", id: request.id, result }));
  }

  /** Return an error for request number `index`. */
  fail(index: number, code: number, message: string): void {
    const request = this.written[index];
    assert.ok(request !== undefined, `request ${index} was never written out`);
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
    userMessage: "hello",
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

/** Let queued microtasks run — used to observe "has the second request gone out yet". */
function tick(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

test("the session id is derived from the platform identity, so the gateway keeps no map", () => {
  assert.equal(sessionIdFor("telegram:42"), "telegram:42");
  // The same person reconnecting lands on the same conversation, across restarts — nothing was stored.
  assert.equal(sessionIdFor("telegram:42"), "telegram:42");
});

test("one turn: the request is right, the reply and the telemetry come back as they were", async () => {
  const { bridge, server } = setup();

  const promise = bridge.ask("telegram:42", "hello");
  await tick();
  assert.equal(server.written.length, 1);

  const request = server.written[0]!;
  assert.equal(request.method, "turn/start");
  assert.equal(request.params["sessionId"], "telegram:42");
  assert.equal(request.params["message"], "hello");
  assert.equal(
    request.params["stream"],
    false,
    "a chat platform wants one whole sentence, not a per-character wobble",
  );

  server.notify(started("t1"));
  server.notify({ type: "toolStarted", turnId: "t1", toolUseId: "tu-1", tool: "memory_search", args: {} });
  server.notify({
    type: "toolCompleted",
    turnId: "t1",
    toolUseId: "tu-1",
    tool: "memory_search",
    output: "three",
    status: "ok",
    durationMs: 3,
  });
  server.notify(completed("t1", "hello there", { model: "big-model", iterations: 2 }));
  server.reply(0, { turnId: "t1" });

  const outcome = await promise;
  assert.equal(outcome.sessionId, "telegram:42");
  assert.equal(outcome.reply, "hello there");
  assert.deepEqual(outcome.tools, ["memory_search"]);
  assert.equal(outcome.model, "big-model");
  assert.equal(outcome.iterations, 2);
});

test("two messages in one conversation: the second waits for the first to finish", async () => {
  const { bridge, server } = setup();

  const first = bridge.ask("telegram:42", "the first");
  const second = bridge.ask("telegram:42", "the second");

  await tick();
  assert.equal(server.written.length, 1, "two messages must not hold the server at once");

  server.notify(started("t1"));
  server.notify(completed("t1", "the first answer"));
  server.reply(0, { turnId: "t1" });
  assert.equal((await first).reply, "the first answer");

  await tick();
  assert.equal(server.written.length, 2, "only once the first is back may the second go out");
  assert.equal(server.written[1]!.params["message"], "the second");

  server.notify(started("t2"));
  server.notify(completed("t2", "the second answer"));
  server.reply(1, { turnId: "t2" });
  assert.equal((await second).reply, "the second answer");
});

test("different conversations do not touch each other", async () => {
  const { bridge, server } = setup();

  // Two people speak at once: queues are per conversation and must not block each other.
  const alice = bridge.ask("telegram:1", "I am alpha");
  const bob = bridge.ask("telegram:2", "I am beta");
  await tick();
  assert.equal(server.written.length, 2, "different conversations must not queue behind each other");

  assert.equal(server.written[0]!.params["sessionId"], "telegram:1");
  assert.equal(server.written[1]!.params["sessionId"], "telegram:2");

  server.notify(started("t1", "telegram:1"));
  server.notify(completed("t1", "hello alpha"));
  server.reply(0, { turnId: "t1" });
  server.notify(started("t2", "telegram:2"));
  server.notify(completed("t2", "hello beta"));
  server.reply(1, { turnId: "t2" });

  assert.equal((await alice).reply, "hello alpha");
  assert.equal((await bob).reply, "hello beta");
});

test("a failed turn: the error comes through and no debris reaches the next turn", async () => {
  const { bridge, server } = setup();

  const first = bridge.ask("telegram:42", "this one will fail");
  await tick();
  server.notify(started("t1"));
  server.notify(completed("t1", "the first turn's answer"));
  // The notification stream finished but the response was an error — the stored copy must be dropped.
  server.fail(0, CODES.INTERNAL_ERROR, "the write failed");
  await assert.rejects(first, /the write failed/);

  const second = bridge.ask("telegram:42", "this one will work");
  await tick();
  server.notify(started("t2"));
  server.notify(completed("t2", "the second turn's answer"));
  server.reply(1, { turnId: "t2" });

  assert.equal((await second).reply, "the second turn's answer", "it must not pick up the previous turn's answer");
});

test("the answer came but the notification chain broke: report it plainly, do not invent an empty reply", async () => {
  const { bridge, server } = setup();

  const promise = bridge.ask("telegram:42", "hello");
  await tick();
  server.notify(started("t1"));
  server.reply(0, { turnId: "t1" });

  await assert.rejects(promise, /never saw turnCompleted/);
});

test("progress: say when a tool starts, and say when one fails too", async () => {
  const notes: string[] = [];
  const { bridge, server } = setup({
    onProgress: (note) => notes.push(note.text),
  });

  const promise = bridge.ask("telegram:42", "look this up for me");
  await tick();
  server.notify(started("t1"));
  server.notify({ type: "toolStarted", turnId: "t1", toolUseId: "tu-read_file", tool: "read_file", args: {} });
  server.notify({
    type: "toolCompleted",
    turnId: "t1",
    toolUseId: "tu-read_file",
    tool: "read_file",
    output: "ENOENT",
    status: "error",
    durationMs: 1,
  });
  // Successful tools are normal, not worth interrupting the user; failures are — the next explanation follows.
  server.notify({ type: "toolStarted", turnId: "t1", toolUseId: "tu-http_get", tool: "http_get", args: {} });
  server.notify({
    type: "toolCompleted",
    turnId: "t1",
    toolUseId: "tu-http_get",
    tool: "http_get",
    output: "200",
    status: "ok",
    durationMs: 20,
  });
  // This one carries no turnId and belongs to no conversation — silently dropped, never guessed.
  server.notify({ type: "consolidationCompleted", newFacts: 2 });
  server.notify(completed("t1", "found it"));
  server.reply(0, { turnId: "t1" });

  await promise;
  assert.deepEqual(notes, [
    "using read_file…",
    "read_file did not succeed; it is trying another way",
    "using http_get…",
  ]);
});

test("errors are put into plain words: no model, not built yet, cannot connect are three different things", () => {
  const provider = new JoyError({
    code: CODES.PROVIDER_ERROR,
    message: "401 Unauthorized",
  });
  assert.match(explain(provider), /joy config/, "it has to tell the user where to fix it");

  const todo = new JoyError({
    code: CODES.NOT_IMPLEMENTED,
    message: "'model/list' is not implemented",
  });
  assert.match(explain(todo), /not built yet/);

  assert.match(explain(new Error("spawn joy ENOENT")), /cannot reach Joy/);
  assert.match(explain("anything at all"), /cannot reach Joy/);
});

test("closing the gateway closes the app-server's stdin", () => {
  const { bridge, server } = setup();
  bridge.close();
  assert.ok(server.closeCalled);
});
