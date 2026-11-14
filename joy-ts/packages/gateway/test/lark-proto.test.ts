import assert from "node:assert/strict";
import test from "node:test";

import {
  METHOD_CONTROL,
  METHOD_DATA,
  decodeFrame,
  emptyFrame,
  encodeFrame,
  headerValue,
} from "../src/lark-proto.ts";
import type { LarkFrame } from "../src/lark-proto.ts";

function frameWith(patch: Partial<LarkFrame>): LarkFrame {
  return { ...emptyFrame(), ...patch };
}

/**
 * A hand-computed byte string, pinning **every field number**.
 *
 * The most important assertion in this file. The biggest risk of a handwritten codec is a wrong
 * field number, whose symptom is "connected but nothing ever arrives" — no error, no stack, just
 * silence. So one ping frame's binary is laid open by hand here: mismatched bytes mean a wrong
 * field number or wire type.
 *
 *   08 01       field 1 (varint) = 1       SeqID
 *   10 00       field 2 (varint) = 0       LogID
 *   18 01       field 3 (varint) = 1       service
 *   20 00       field 4 (varint) = 0       method (control frame)
 *   2a 0c       field 5 (bytes) len 12     headers[0]
 *     0a 04 74 79 70 65   field 1 = "type"
 *     12 04 70 69 6e 67   field 2 = "ping"
 */
const PING_BYTES = [
  0x08, 0x01,
  0x10, 0x00,
  0x18, 0x01,
  0x20, 0x00,
  0x2a, 0x0c,
  0x0a, 0x04, 0x74, 0x79, 0x70, 0x65,
  0x12, 0x04, 0x70, 0x69, 0x6e, 0x67,
];

test("encode: the bytes of one ping frame are the hand-computed ones", () => {
  const frame = frameWith({
    seqId: 1n,
    service: 1,
    method: METHOD_CONTROL,
    headers: [{ key: "type", value: "ping" }],
  });
  assert.deepEqual([...encodeFrame(frame)], PING_BYTES);
});

test("decode: the hand-computed bytes read back the same", () => {
  const frame = decodeFrame(Uint8Array.from(PING_BYTES));
  assert.ok(frame !== null);
  assert.equal(frame.seqId, 1n);
  assert.equal(frame.logId, 0n);
  assert.equal(frame.service, 1);
  assert.equal(frame.method, METHOD_CONTROL);
  assert.deepEqual(frame.headers, [{ key: "type", value: "ping" }]);
  assert.equal(frame.payload, undefined);
});

test("round trip: a data frame brings back everything it should", () => {
  const payload = new TextEncoder().encode('{"hello":"world"}');
  const frame = frameWith({
    seqId: 42n,
    logId: 7n,
    service: 33_554_678,
    method: METHOD_DATA,
    headers: [
      { key: "type", value: "event" },
      { key: "message_id", value: "om_abc" },
      { key: "sum", value: "1" },
      { key: "seq", value: "0" },
    ],
    payload,
  });

  const back = decodeFrame(encodeFrame(frame));
  assert.ok(back !== null);
  assert.equal(back.seqId, 42n);
  assert.equal(back.logId, 7n);
  assert.equal(back.service, 33_554_678);
  assert.equal(back.method, METHOD_DATA);
  assert.deepEqual(back.headers, frame.headers);
  assert.deepEqual(back.payload, payload);
});

test("round trip: the header order is kept", () => {
  // The ACK echoes the original frame whole, so a wrong order still "matches" — but sorted
  // headers would make one suspect a dropped entry. Order is preserved; this nails it down.
  const headers = [
    { key: "a", value: "1" },
    { key: "b", value: "2" },
    { key: "a", value: "3" },
  ];
  const back = decodeFrame(encodeFrame(frameWith({ headers })));
  assert.ok(back !== null);
  assert.deepEqual(back.headers, headers);
});

test("round trip: fields arriving out of order are still recognised", () => {
  // protobuf doesn't require fields in numeric order. Whatever the server's order, we must read it.
  const shuffled = Uint8Array.from([
    0x20, 0x01, // method = 1
    0x18, 0x0e, // service = 14
    0x08, 0x05, // seqId = 5
  ]);
  const frame = decodeFrame(shuffled);
  assert.ok(frame !== null);
  assert.equal(frame.method, METHOD_DATA);
  assert.equal(frame.service, 14);
  assert.equal(frame.seqId, 5n);
});

test("round trip: a logId past 2^53 keeps its precision", () => {
  // This is the field the ACK must backfill verbatim. Held in a `number`, it would already have
  // silently changed here — backfilling an altered id is invisible daily and untraceable in incidents.
  const big = 18_446_744_073_709_551_615n; // 2^64 - 1
  // At this magnitude a `number` can't even remember adding one — that's how precision is lost.
  assert.equal(Number(big) + 1, Number(big));

  const back = decodeFrame(encodeFrame(frameWith({ logId: big })));
  assert.ok(back !== null);
  assert.equal(back.logId, big);
});

test("varint: the boundary values all come out right", () => {
  for (const seqId of [0n, 1n, 127n, 128n, 300n, 16_383n, 16_384n]) {
    const back = decodeFrame(encodeFrame(frameWith({ seqId })));
    assert.ok(back !== null);
    assert.equal(back.seqId, seqId);
  }
});

test("a negative service is truncated back as int32", () => {
  // The protocol's service is positive, but the encoding of negatives (10 bytes, sign-extended
  // to 64 bits) must be recognized — otherwise malformed/misaligned input decodes to a huge positive.
  const back = decodeFrame(encodeFrame(frameWith({ service: -1 })));
  assert.ok(back !== null);
  assert.equal(back.service, -1);
  // -1 encodes as a 10-byte varint (sign-extended to 64 bits), 11 with the tag; the other three
  // zero fields take 2 each. A "plain varint" write would give only 8 — that's the difference.
  assert.equal(encodeFrame(frameWith({ service: -1 })).length, 17);
});

test("an unrecognised field: skip it and read the rest", () => {
  // The day Lark adds a field must not look like "connected, then suddenly nothing". Protobuf's
  // own rule is to skip. Three wire types of unknown fields are stuffed in here.
  const extra = Uint8Array.from([
    0x50, 0x2a, // field 10, varint
    0x59, 1, 2, 3, 4, 5, 6, 7, 8, // field 11, fixed64
    0x65, 1, 2, 3, 4, // field 12, fixed32
  ]);
  const frame = decodeFrame(
    Uint8Array.from([...PING_BYTES, ...extra, 0x08, 0x02]),
  );
  assert.ok(frame !== null);
  assert.equal(frame.method, METHOD_CONTROL);
  assert.deepEqual(frame.headers, [{ key: "type", value: "ping" }]);
  // After skipping, reading must continue: the final seqId here overrides the earlier 1.
  assert.equal(frame.seqId, 2n);
});

test("a malformed frame returns null instead of throwing", () => {
  // One bad frame must not take the whole connection down — the caller logs, drops, reads on.
  const cases: Array<[string, number[]]> = [
    ["only a tag left", [0x08]],
    ["a varint cut short", [0x08, 0x80]],
    ["a varint past 10 bytes", [0x08, ...Array<number>(11).fill(0x80), 0x01]],
    ["a length prefix pointing past the array", [0x2a, 0x7f, 0x01, 0x02]],
    ["a header whose length lies", [0x2a, 0x03, 0x0a, 0x09, 0x41]],
    ["an unrecognised wire type", [0x0f, 0x01]],
  ];

  for (const [what, bytes] of cases) {
    assert.equal(decodeFrame(Uint8Array.from(bytes)), null, what);
  }
});

test("zero bytes read as «all defaults», not null", () => {
  // Not malformed — in protobuf an empty message is the legal encoding of "every field default".
  // The adapter needs no special case: `method` is 0 and there's no `type`, so it's ignored on the way through.
  const frame = decodeFrame(new Uint8Array(0));
  assert.ok(frame !== null);
  assert.deepEqual(frame, emptyFrame());
});

test("when the length prefix lies, do not give me a frame at all", () => {
  // Deliberate design: half a frame would be used to ACK the wrong thing — worse than dropping.
  const good = encodeFrame(
    frameWith({ headers: [{ key: "type", value: "event" }] }),
  );
  const frame = decodeFrame(good);
  assert.ok(frame !== null);

  // Only the first half of it is correct.
  assert.equal(decodeFrame(good.subarray(0, good.length - 1)), null);
});

test("an empty payload really means there is none", () => {
  // In proto3 empty bytes don't go on the wire; it reads back as undefined, not an empty array —
  // the adapter's `payload === undefined` check depends on exactly this.
  const back = decodeFrame(
    encodeFrame(frameWith({ payload: new Uint8Array(0) })),
  );
  assert.ok(back !== null);
  assert.equal(back.payload, undefined);
});

test("headerValue: the first one, or an empty string when absent", () => {
  const frame = frameWith({
    headers: [
      { key: "type", value: "event" },
      { key: "type", value: "card" },
    ],
  });
  assert.equal(headerValue(frame, "type"), "event");
  assert.equal(headerValue(frame, "message_id"), "");
});

test("bad bytes do not blow up: text fields decode lossily", () => {
  // Same attitude as the official SDK — one bad byte becomes a replacement character instead of
  // taking the connection down. These bytes are invalid UTF-8 in a header (field 5) key's start.
  const frame = decodeFrame(
    Uint8Array.from([0x2a, 0x06, 0x0a, 0x04, 0xff, 0xfe, 0x74, 0x65]),
  );
  assert.ok(frame !== null);
  assert.equal(frame.headers.length, 1);
  assert.equal(frame.headers[0]?.key, "\u{fffd}\u{fffd}te");
});
