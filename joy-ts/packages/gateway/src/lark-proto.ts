/**
 * The frame format (`pbbp2`) running over Lark's long-connection WebSocket.
 *
 * This is **Lark's private protobuf**: the official `.proto` isn't published on the docs site;
 * it only exists inside the vendors' SDK artifacts. So the codec is handwritten against the
 * protobuf wire format — this one message only, no protobuf runtime (same reason this project
 * doesn't pull in `discord.js`: a code generator for one message costs more than it buys).
 *
 * Field numbers were cross-checked against three independent implementations, which agree:
 *
 * | # | field | type |
 * |---|---|---|
 * | 1 | `SeqID` | uint64 |
 * | 2 | `LogID` | uint64 |
 * | 3 | `service` | int32 |
 * | 4 | `method` | int32 |
 * | 5 | `headers` | repeated Header |
 * | 6 | `payload_encoding` | string |
 * | 7 | `payload_type` | string |
 * | 8 | `payload` | bytes |
 * | 9 | `LogIDNew` | string |
 *
 * 6/7 exist for non-JSON payloads (`gzip`, binary…). Lark currently sends JSON only, so this
 * version keeps them as plain strings and doesn't implement decompression.
 *
 * The price of a handwritten codec: a wrong field number tells nobody — the symptom is
 * "connected but nothing ever arrives". So everything in this file is **pure functions**,
 * pinned by tests; the adapter only handles networking. That's also why it's a separate file.
 */

/** The few wire types we'll ever use. */
const WIRE_VARINT = 0;
const WIRE_FIXED64 = 1;
const WIRE_BYTES = 2;
const WIRE_FIXED32 = 5;

/** Values of `method`. Control frames are pings; data frames are events. */
export const METHOD_CONTROL = 0;
export const METHOD_DATA = 1;

/** A key/value pair in a frame. `type`, `message_id`, `sum`, `seq` all come from here. */
export interface LarkFrameHeader {
  key: string;
  value: string;
}

export interface LarkFrame {
  /**
   * 1, uint64. Self-incrementing for our own requests; the server **echoes it back verbatim**.
   *
   * `bigint` rather than `number`: this is uint64, and the ACK must backfill the server's
   * `logId` **verbatim**. Past 2^53 a `number` silently loses precision; backfilling an altered
   * id is the error that's invisible daily and untraceable in an incident. Lark knows this trap
   * too — that's why field 9, the string version, exists.
   */
  seqId: bigint;
  /** 2, uint64. The server's log id. See above for why this is a `bigint`. */
  logId: bigint;
  /** 3, int32. The number parsed from the wss URL's `service_id` at handshake. */
  service: number;
  /** 4, int32. `METHOD_CONTROL` or `METHOD_DATA`. */
  method: number;
  /** 5, repeated Header. */
  headers: LarkFrameHeader[];
  /** 6, string. Payload encoding; see the file-header comment. */
  payloadEncoding: string;
  /** 7, string. Payload type. */
  payloadType: string;
  /** 8, bytes. In a data frame this is the JSON blob. */
  payload: Uint8Array | undefined;
  /** 9, string. String log id, for languages that can't hold a uint64. */
  logIdNew: string;
}

/** An empty shell before any frame exists. `decodeFrame` starts filling from here. */
export function emptyFrame(): LarkFrame {
  return {
    seqId: 0n,
    logId: 0n,
    service: 0,
    method: 0,
    headers: [],
    payloadEncoding: "",
    payloadType: "",
    payload: undefined,
    logIdNew: "",
  };
}

/** Value of the first header with that name. Empty if absent — no `undefined` checks for callers. */
export function headerValue(frame: LarkFrame, key: string): string {
  for (const header of frame.headers) {
    if (header.key === key) return header.value;
  }
  return "";
}

// ---- Write --------------------------------------------------------------------

function pushVarint(out: number[], raw: bigint): void {
  // Negative numbers in protobuf are **sign-extended to 64 bits** before encoding (so int32's -1
  // is a 10-byte varint). Without this step, the `while` below loops forever on a negative.
  let rest = raw < 0n ? raw + (1n << 64n) : raw;
  while (rest > 0x7fn) {
    out.push(Number((rest & 0x7fn) | 0x80n));
    rest >>= 7n;
  }
  out.push(Number(rest));
}

function pushTag(out: number[], field: number, wire: number): void {
  pushVarint(out, BigInt((field << 3) | wire));
}

function pushBytes(out: number[], field: number, value: Uint8Array): void {
  pushTag(out, field, WIRE_BYTES);
  pushVarint(out, BigInt(value.length));
  for (const byte of value) out.push(byte);
}

/** proto3's rule: defaults (empty string, empty bytes) don't go on the wire. */
function pushString(out: number[], field: number, value: string): void {
  if (value === "") return;
  pushBytes(out, field, new TextEncoder().encode(value));
}

function encodeHeader(header: LarkFrameHeader): Uint8Array {
  const out: number[] = [];
  pushString(out, 1, header.key);
  pushString(out, 2, header.value);
  return Uint8Array.from(out);
}

/**
 * Encode one frame into binary.
 *
 * Fields 1–4 are **written even when 0**: they carry meaning here (`logId = 0` is "no log id
 * yet", not "not said"), and writing them always makes matching easier.
 */
export function encodeFrame(frame: LarkFrame): Uint8Array {
  const out: number[] = [];
  pushTag(out, 1, WIRE_VARINT);
  pushVarint(out, frame.seqId);
  pushTag(out, 2, WIRE_VARINT);
  pushVarint(out, frame.logId);
  pushTag(out, 3, WIRE_VARINT);
  pushVarint(out, BigInt(frame.service));
  pushTag(out, 4, WIRE_VARINT);
  pushVarint(out, BigInt(frame.method));
  for (const header of frame.headers) {
    pushBytes(out, 5, encodeHeader(header));
  }
  pushString(out, 6, frame.payloadEncoding);
  pushString(out, 7, frame.payloadType);
  if (frame.payload !== undefined && frame.payload.length > 0) {
    pushBytes(out, 8, frame.payload);
  }
  pushString(out, 9, frame.logIdNew);
  return Uint8Array.from(out);
}

// ---- Read --------------------------------------------------------------------

/** Where we've read to. One small mutable object beats passing an offset down every layer. */
interface Reader {
  readonly bytes: Uint8Array;
  at: number;
}

function readVarint(reader: Reader): bigint | null {
  let value = 0n;
  let shift = 0n;
  while (reader.at < reader.bytes.length) {
    const byte = reader.bytes[reader.at] as number;
    reader.at += 1;
    value |= BigInt(byte & 0x7f) << shift;
    if ((byte & 0x80) === 0) return value;
    shift += 7n;
    // 64 bits is at most 10 bytes. More is malformed — don't let it read on forever.
    if (shift >= 70n) return null;
  }
  return null;
}

function readBytes(reader: Reader): Uint8Array | null {
  const length = readVarint(reader);
  if (length === null) return null;
  // The length is declared up front: a lying length must not push the read pointer out of bounds.
  if (length > BigInt(reader.bytes.length - reader.at)) return null;
  const size = Number(length);
  const slice = reader.bytes.subarray(reader.at, reader.at + size);
  reader.at += size;
  return slice;
}

function advance(reader: Reader, count: number): boolean {
  if (reader.at + count > reader.bytes.length) return false;
  reader.at += count;
  return true;
}

/**
 * Unrecognized wire types come here: **skip** them instead of giving up on the whole frame.
 *
 * A deliberate choice. Breaking on an unknown field means that the day Lark adds a fixed-size
 * field, the symptom is "connected, then suddenly nothing" — and not one frame readable, not
 * even a log line. Protobuf's own rule is to skip.
 */
function skipField(reader: Reader, wire: number): boolean {
  switch (wire) {
    case WIRE_VARINT:
      return readVarint(reader) !== null;
    case WIRE_FIXED64:
      return advance(reader, 8);
    case WIRE_FIXED32:
      return advance(reader, 4);
    default:
      // 3/4 are the deprecated group types; 6/7 aren't legal wire types at all.
      return false;
  }
}

function decodeText(bytes: Uint8Array): string {
  // Lossy decoding by default (bad bytes become U+FFFD) rather than throwing, same attitude as
  // the official SDK: one bad byte in a text frame shouldn't take the whole connection down.
  return new TextDecoder().decode(bytes);
}

function decodeHeader(bytes: Uint8Array): LarkFrameHeader | null {
  const header: LarkFrameHeader = { key: "", value: "" };
  const reader: Reader = { bytes, at: 0 };
  while (reader.at < bytes.length) {
    const tag = readVarint(reader);
    if (tag === null) return null;
    const field = Number(tag >> 3n);
    const wire = Number(tag & 0x7n);
    if (wire === WIRE_BYTES) {
      const slice = readBytes(reader);
      if (slice === null) return null;
      if (field === 1) header.key = decodeText(slice);
      else if (field === 2) header.value = decodeText(slice);
      continue;
    }
    if (!skipField(reader, wire)) return null;
  }
  return header;
}

/**
 * Decode one frame. **Unrecognizable means `null`, not a throw** — a malformed frame must not be
 * able to take the connection down; the caller logs one line, drops it, reads the next.
 *
 * Always "all or nothing": half a frame would be used to ACK the wrong thing — worse than dropping.
 */
export function decodeFrame(bytes: Uint8Array): LarkFrame | null {
  const frame = emptyFrame();
  const reader: Reader = { bytes, at: 0 };

  while (reader.at < bytes.length) {
    const tag = readVarint(reader);
    if (tag === null) return null;
    const field = Number(tag >> 3n);
    const wire = Number(tag & 0x7n);

    if (wire === WIRE_VARINT) {
      const value = readVarint(reader);
      if (value === null) return null;
      switch (field) {
        case 1:
          frame.seqId = value;
          break;
        case 2:
          frame.logId = value;
          break;
        // int32 negatives on the wire are sign-extended 64-bit; truncate them back.
        case 3:
          frame.service = Number(BigInt.asIntN(32, value));
          break;
        case 4:
          frame.method = Number(BigInt.asIntN(32, value));
          break;
        default:
          break;
      }
      continue;
    }

    if (wire === WIRE_BYTES) {
      const slice = readBytes(reader);
      if (slice === null) return null;
      switch (field) {
        case 5: {
          const header = decodeHeader(slice);
          if (header === null) return null;
          frame.headers.push(header);
          break;
        }
        case 6:
          frame.payloadEncoding = decodeText(slice);
          break;
        case 7:
          frame.payloadType = decodeText(slice);
          break;
        case 8:
          // No copy: this is a view on the original buffer. Saves a memcpy; the price is not
          // repurposing that buffer for something else.
          frame.payload = slice;
          break;
        case 9:
          frame.logIdNew = decodeText(slice);
          break;
        default:
          break;
      }
      continue;
    }

    if (!skipField(reader, wire)) return null;
  }

  return frame;
}
