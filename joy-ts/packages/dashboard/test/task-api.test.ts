import assert from "node:assert/strict";
import { test } from "node:test";
import { taskAction, taskOperations, reconcileTaskOperation, turnOperations, reconcileTurnOperation } from "../src/api.ts";

test("a task refusal carried by HTTP 200 is still an error", async () => {
  const originalFetch = globalThis.fetch;
  const originalStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: { getItem: () => null },
  });
  globalThis.fetch = async () => new Response(JSON.stringify({
    code: -32602,
    message: "Task execution stopped: input deadline has expired",
  }), { status: 200, headers: { "content-type": "application/json" } });
  try {
    await assert.rejects(
      taskAction({ action: "start", taskId: "task", message: "Prepare the report" }),
      /input deadline has expired/,
    );
    await assert.rejects(taskOperations("task"), /input deadline has expired/);
    await assert.rejects(turnOperations({sessionId:"chat"}), /input deadline has expired/);
    await assert.rejects(reconcileTurnOperation({sessionId:"chat",turnId:"turn",operationId:"op",checkpointDigest:"digest",resolution:"not_executed",evidence:"Human checked"}), /input deadline has expired/);
    await assert.rejects(reconcileTaskOperation({ taskId: "task", operationId: "op",
      checkpointDigest: "digest", resolution: "not_executed", evidence: "Checked external state" }), /input deadline has expired/);
  } finally {
    globalThis.fetch = originalFetch;
    if (originalStorage) Object.defineProperty(globalThis, "localStorage", originalStorage);
    else Reflect.deleteProperty(globalThis, "localStorage");
  }
});
