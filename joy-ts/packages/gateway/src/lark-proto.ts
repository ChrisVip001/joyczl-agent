/**
 * 飞书长连接那根 WebSocket 上跑的帧格式（`pbbp2`）。
 *
 * 这是**飞书私有的 protobuf**：官方的 `.proto` 没放在文档站上，只存在于各家
 * SDK 的生成物里。所以这里按 protobuf 的 wire format 手写编解码 —— 只解这一个
 * 消息，不引 protobuf 运行时（跟这个项目不引 `discord.js` 是同一条理由：
 * 为了一个消息拖进来一个代码生成器，代价比收益大）。
 *
 * 字段号是从三个互相独立的实现里对出来的，三者一致：
 *
 * | 号 | 字段 | 类型 |
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
 * 6/7 是给非 JSON 负载留的（`gzip`、二进制之类）。飞书现在发的都是 JSON，
 * 所以这一版解出来就直接当字符串留着，不实现解压。
 *
 * 手写编解码的代价是：字段号错了没人会告诉你，表现是「连上了但什么都不来」。
 * 所以这个文件里的东西**一律是纯函数**，拿测试把它钉死；适配器那边只管连网。
 * 这也是它单独一个文件的原因。
 */

/** 只会用到的几个 wire type。 */
const WIRE_VARINT = 0;
const WIRE_FIXED64 = 1;
const WIRE_BYTES = 2;
const WIRE_FIXED32 = 5;

/** `method` 的取值。控制帧是心跳，数据帧是事件。 */
export const METHOD_CONTROL = 0;
export const METHOD_DATA = 1;

/** 帧里的一对 key/value。`type`、`message_id`、`sum`、`seq` 都从这儿取。 */
export interface LarkFrameHeader {
  key: string;
  value: string;
}

export interface LarkFrame {
  /**
   * 1，uint64。自己发的请求自增，服务端**原样回填** —— 靠它把回执对上是哪一次。
   *
   * 用 `bigint` 而不是 `number`：这是 uint64，而 ACK 要把服务端给的 `logId`
   * **原封不动**回填。在 `number` 里超过 2^53 就开始悄悄丢精度，回填一个被改
   * 过的 id 是那种平时看不出、出事时查不动的错。飞书自己也知道这个坑 ——
   * 所以才有了字段 9 那个字符串版。
   */
  seqId: bigint;
  /** 2，uint64。服务端的日志 id。见上面那条为什么是 `bigint`。 */
  logId: bigint;
  /** 3，int32。握手时从 wss 地址的 `service_id` 解出来的那个数。 */
  service: number;
  /** 4，int32。`METHOD_CONTROL` 或 `METHOD_DATA`。 */
  method: number;
  /** 5，repeated Header。 */
  headers: LarkFrameHeader[];
  /** 6，string。负载编码，见文件头那条注释。 */
  payloadEncoding: string;
  /** 7，string。负载类型。 */
  payloadType: string;
  /** 8，bytes。数据帧里就是那段 JSON。 */
  payload: Uint8Array | undefined;
  /** 9，string。字符串版日志 id，专门给拿不住 uint64 的语言用。 */
  logIdNew: string;
}

/** 一帧都还没有的空壳。`decodeFrame` 从它开始往里填。 */
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

/** 取第一个同名 header 的值。没有就是空串 —— 调用方不必到处判 `undefined`。 */
export function headerValue(frame: LarkFrame, key: string): string {
  for (const header of frame.headers) {
    if (header.key === key) return header.value;
  }
  return "";
}

// ---- 写 --------------------------------------------------------------------

function pushVarint(out: number[], raw: bigint): void {
  // protobuf 里的负数先**符号扩展到 64 位**再编码（所以 int32 的 -1 是 10 个
  // 字节的 varint）。少了这一步，下面那个 `while` 会拿着负数一直循环。
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

/** proto3 的规矩：默认值（空串、空 bytes）不写进线里。 */
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
 * 把一帧编成一串二进制。
 *
 * 1–4 号字段**无论是不是 0 都照写**。省掉它们能少几个字节，但协议里这几个
 * 是有含义的（`logId = 0` 是「还没有日志号」，不是「没说」），写全了更好对。
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

// ---- 读 --------------------------------------------------------------------

/** 读到哪儿了。用一个可变的小对象，比一层层传 offset 干净。 */
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
    // 64 位最多 10 个字节。再多就是畸形数据 —— 不能由着它一直读下去。
    if (shift >= 70n) return null;
  }
  return null;
}

function readBytes(reader: Reader): Uint8Array | null {
  const length = readVarint(reader);
  if (length === null) return null;
  // 长度是先声明的：一个说谎的长度不能把读指针推到数组外面去。
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
 * 认不出的 wire type 走这儿：**跳过**它，而不是就地放弃整帧。
 *
 * 这是有意的选择。碰到不认识的字段就 `break` 的话，飞书哪天加一个定长字段，
 * 表现就是「连着连着突然什么都不来了」—— 而且一帧都读不出来，连日志都记不下。
 * protobuf 自己的规矩就是跳过。
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
      // 3/4 是废弃的 group；6/7 根本不是合法的 wire type。
      return false;
  }
}

function decodeText(bytes: Uint8Array): string {
  // 默认就是有损解码（坏字节换成 U+FFFD）而不是抛异常，跟官方 SDK 一个态度：
  // 一帧文本里有个坏字节，不该把整条连接带下去。
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
 * 解一帧。**认不出就是 `null`，不抛** —— 畸形的帧不该有能力把连接带走，
 * 调用方记一行日志、丢掉它，接着读下一帧。
 *
 * 一律「全解出来或全不给」：半截的帧会被拿去 ACK 一个错的东西，比丢掉更糟。
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
        // int32 的负数在线里是符号扩展过的 64 位，得截回来。
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
          // 不拷贝：这是原缓冲区上的一个视图。省一次 memcpy，代价是别把那块
          // 缓冲区改成别的东西用。
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
