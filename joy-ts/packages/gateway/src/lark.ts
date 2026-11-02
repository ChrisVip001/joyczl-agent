import { explain } from "./bridge.ts";
import type { Responder } from "./bridge.ts";
import { splitMessage } from "./text.ts";
import {
  METHOD_CONTROL,
  METHOD_DATA,
  decodeFrame,
  emptyFrame,
  encodeFrame,
  headerValue,
} from "./lark-proto.ts";
import type { LarkFrame, LarkFrameHeader } from "./lark-proto.ts";

export type { Responder } from "./bridge.ts";

/**
 * 飞书（Lark）适配器 —— **长连接**，不用公网地址。
 *
 * 这是四家里协议最别扭的一家，别扭在两处：
 *
 * 1. **帧不是 JSON，是飞书私有的 protobuf**（`pbbp2`）。所以旁边多了一个
 *    `lark-proto.ts`：只解这一个消息的手写编解码。官方给的路子是引 SDK，而这个
 *    项目从 Discord 那家起就没引过平台的 SDK —— 为了一个消息拖进来一整个
 *    protobuf 运行时，代价比收益大。
 * 2. **要先拿地址再去连**。不是打开一个固定的 wss 地址就完事：得先拿
 *    AppID/AppSecret 换一个**带一次性票据的**地址，再连那个。换来的地址里有
 *    `service_id`，之后每一帧都得带上它。
 *
 * 顺带一个跟微信正好相反的对照：**飞书把「先应答、后回答」写成了明文规矩**
 * （收到数据帧要在 3 秒内回一个 ACK，答复另走 HTTP 发）。微信那边我们只能自己
 * 掐一个 4 秒的表。所以这里没有竞速 —— 收到帧就先把 ACK 拍回去，然后慢慢想。
 */

/**
 * 一条消息的字数上限。
 *
 * 飞书文本消息的请求体上限是 **150KB**，超了报 `230025`。30k 字符在中文下约
 * 90KB，留了余量。这只是个「有界」的选择而不是踩过的墙 —— 官方文档的发送消息
 * 那页并没有写这个数。
 */
export const MESSAGE_LIMIT = 30_000;

/** 飞书，以及国际版 Lark。两个域的接口路径一模一样，只有域名不同。 */
const DOMAINS = {
  feishu: { api: "https://open.feishu.cn/open-apis", ws: "https://open.feishu.cn" },
  lark: { api: "https://open.larksuite.com/open-apis", ws: "https://open.larksuite.com" },
} as const;

/** 服务端没给心跳间隔时的默认值。 */
const DEFAULT_PING_MS = 120_000;

/**
 * 心跳间隔的下限。**这个有据可依**：官方 SDK 里写着小于 10 秒的要「静默忽略」，
 * 也就是照旧用默认值 —— 服务端算下一轮心跳是按自己那个间隔来的，改得比这更勤
 * 只会把轮次搅乱。
 */
const MIN_PING_MS = 10_000;

/** ACK 的负载。固定长这样，两个第三方实现给的都是这一串。 */
const ACK_PAYLOAD = new TextEncoder().encode('{"code":200,"headers":{},"data":[]}');

/** 记多久的 `message_id` 用来去重。飞书重发不会隔半小时那么久。 */
const SEEN_TTL_MS = 30 * 60 * 1000;

/** 分片的半成品留多久。到点了直接扔 —— 少一片的事件永远凑不齐。 */
const FRAGMENT_TTL_MS = 5 * 60 * 1000;

/** 重连退避的封顶。跟 Discord 那家同一个数。 */
const MAX_PAUSE_MS = 30_000;

/**
 * 多久没收到任何东西就当这根连接已经死了。
 *
 * 半开的 TCP 连接（合上笔记本、NAT 超时）不会报错，只会安静地不再送来任何东西 ——
 * 这是长连接最恶心的一种坏法：进程还在，用户以为它还活着。两个第三方实现都用
 * 300 秒；这里跟着心跳间隔走，服务端把间隔调大时也还成立。
 */
function idleTimeoutMs(pingMs: number): number {
  return Math.max(300_000, pingMs * 2 + 60_000);
}

/**
 * 从握手换来的地址里解出 `service_id`。
 *
 * 之后每一帧的 3 号字段都得填它，填错了服务端当没听见 —— 又一个安静的失败。
 * 所以单独一个函数、单独测。
 */
export function serviceIdOf(url: string): number {
  let raw: string | null;
  try {
    raw = new URL(url).searchParams.get("service_id");
  } catch {
    return 0;
  }
  if (raw === null || raw === "") return 0;
  const value = Number(raw);
  return Number.isSafeInteger(value) ? value : 0;
}

/** 心跳帧：控制帧，身上只挂一个 `type: ping`。`seqId` 是自己发出去的序号。 */
export function buildPing(service: number, seqId: bigint): LarkFrame {
  return {
    ...emptyFrame(),
    seqId,
    service,
    method: METHOD_CONTROL,
    headers: [{ key: "type", value: "ping" }],
  };
}

/**
 * ACK 帧：把收到的那一帧**整体回显**，只换两处 —— 负载换成固定的那串 JSON，
 * 再补一个 `biz_rt`。
 *
 * 是「回显」而不是「新造一帧」：帧里的 `seqId` 和 `logId` 是服务端用来把这次
 * 应答对上是哪个请求的，编一个新号就没对上了。所以连 headers 的顺序都留着。
 *
 * **不改传进来的那帧** —— 它后面还要用来读事件，顺手改掉的话会在离现场很远的
 * 地方出问题。
 */
export function buildAck(frame: LarkFrame): LarkFrame {
  const headers: LarkFrameHeader[] = [...frame.headers];
  if (!headers.some((header) => header.key === "biz_rt")) {
    headers.push({ key: "biz_rt", value: "0" });
  }
  return { ...frame, headers, payload: ACK_PAYLOAD };
}

/** 只认这一种事件。别的（卡片回调、入群、撤回）先不接。 */
const MESSAGE_EVENT = "im.message.receive_v1";

/**
 * 把正文里那些 `@_user_1` 占位符抹掉。
 *
 * 飞书在 `content.text` 里放的是占位符，真名在 `mentions` 里。占位符对模型来说
 * 是噪音，直接去掉 —— 两个第三方实现也是这么干的。
 */
export function stripMentionKeys(text: string): string {
  return text
    .replace(/@_user_\d+/g, "")
    .replace(/\s{2,}/g, " ")
    .trim();
}

/**
 * pong 里可能夹带新的心跳间隔（服务端自己调）。
 *
 * 返回 `null` 是「这次没说」，跟「说了 0 秒」是两回事 —— 分得开才敢照着改。
 *
 * **小于 10 秒的一律当没说**（不是夹到 10 秒）：官方 SDK 对这种情况就是「静默
 * 忽略、照旧用默认值」。这个数服务端是按自己那一轮算的，夹一下反而会把轮次
 * 搅乱 —— 客气地当成没提，比自己改一个数安全。
 */
export function pingIntervalOf(frame: LarkFrame): number | null {
  if (frame.payload === undefined || frame.payload.length === 0) return null;
  try {
    const parsed = JSON.parse(new TextDecoder().decode(frame.payload)) as {
      ClientConfig?: { PingInterval?: unknown };
    };
    const seconds = parsed.ClientConfig?.PingInterval;
    if (typeof seconds !== "number" || !Number.isFinite(seconds)) return null;
    if (seconds < MIN_PING_MS / 1000) return null;
    return seconds * 1000;
  } catch {
    return null;
  }
}

/** 取一个 header 上的整数。空串和垃圾都当 0。 */
function headerInt(frame: LarkFrame, key: string): number {
  const raw = headerValue(frame, key);
  if (raw === "") return 0;
  const value = Number(raw);
  return Number.isSafeInteger(value) ? value : 0;
}

/** 把一帧的 `data` 收成字节。不是二进制就返回 `null`。 */
function toBytes(data: unknown): Uint8Array | null {
  if (data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) {
    return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  }
  return null;
}

function concat(parts: readonly Uint8Array[]): Uint8Array {
  const total = parts.reduce((sum, part) => sum + part.length, 0);
  const out = new Uint8Array(total);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// ---- 连出来的那根 socket ---------------------------------------------------

/**
 * 连接的最小形状。跟 Discord 那家一样，是为了测试里能塞一个假的进去 ——
 * 它只有 WebSocket 的那几个回调和两个方法。
 */
export interface LarkSocket {
  send(data: string | Uint8Array): void;
  close(): void;
  onopen: (() => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
  /**
   * 二进制帧要以 `ArrayBuffer` 递过来。
   *
   * 这不是可有可无的：WHATWG 的 WebSocket 默认给的是 **Blob**，而 Blob 只能
   * 异步读。真按默认值走，表现是「连上了、一帧都读不出来」—— 又一次安静的失败。
   * 默认工厂会把它设成 `arraybuffer`，万一还是收到 Blob 也会明确喊一声。
   */
  binaryType?: string;
}

export type LarkSocketFactory = (url: string) => LarkSocket;

export interface LarkAdapterOptions {
  appId: string;
  appSecret: string;
  /** 白名单：只有这些 `open_id` 说的话才理。不填 = 谁都理。 */
  allow?: readonly string[];
  /** 连国际版 Lark（`open.larksuite.com`）而不是飞书。 */
  lark?: boolean;
  fetch?: typeof globalThis.fetch;
  socket?: LarkSocketFactory;
  onLog?: (text: string) => void;
}

interface LarkMention {
  key?: string;
  name?: string;
  id?: { open_id?: string; union_id?: string; user_id?: string };
}

interface LarkEvent {
  header?: { event_type?: string; event_id?: string };
  event?: {
    sender?: {
      sender_type?: string;
      sender_id?: { open_id?: string; union_id?: string; user_id?: string };
    };
    message?: {
      message_id?: string;
      chat_id?: string;
      chat_type?: string;
      message_type?: string;
      content?: string;
      mentions?: LarkMention[];
    };
  };
}

export class LarkAdapter {
  readonly #appId: string;
  readonly #appSecret: string;
  readonly #allow: ReadonlySet<string>;
  readonly #apiBase: string;
  readonly #wsBase: string;
  readonly #fetch: typeof globalThis.fetch;
  readonly #open: LarkSocketFactory;
  readonly #respond: Responder;
  readonly #onLog: ((text: string) => void) | undefined;

  #socket: LarkSocket | undefined;
  #service = 0;
  #seq = 0n;
  #stopped = false;
  #failures = 0;

  /** 收到读不了的帧时，每次连接只喊一声，别刷屏。 */
  #warnedFrame = false;

  /** 已经处理过的 `message_id` → 处理时间。飞书重发时靠它跳过。 */
  readonly #seen = new Map<string, number>();

  /** 攒到一半的分片。`message_id` → 各片。 */
  readonly #fragments = new Map<string, { parts: Array<Uint8Array | undefined>; at: number }>();

  /** `tenant_access_token` 有 2 小时，缓存起来，别每条消息都去要。 */
  #token: { value: string; until: number } | undefined;

  /** 机器人自己的 `open_id`，用来判断群里是不是在叫它。`null` = 还没问过。 */
  #selfId: string | undefined | null = null;
  #warnedSelf = false;

  constructor(options: LarkAdapterOptions, respond: Responder) {
    this.#appId = options.appId;
    this.#appSecret = options.appSecret;
    this.#allow = new Set(options.allow ?? []);
    const domain = options.lark === true ? DOMAINS.lark : DOMAINS.feishu;
    this.#apiBase = domain.api;
    this.#wsBase = domain.ws;
    this.#fetch = options.fetch ?? globalThis.fetch;
    this.#open = options.socket ?? defaultSocketFactory;
    this.#respond = respond;
    this.#onLog = options.onLog;
  }

  #log(text: string): void {
    this.#onLog?.(text);
  }

  /** 一直连到被叫停。断了就重连，退避翻倍、封顶 30 秒。 */
  async run(): Promise<void> {
    while (!this.#stopped) {
      try {
        await this.#connect();
        // 连上过就把之前的失败记录清零：一次断线不该让下一次重连也等很久。
        this.#failures = 0;
        if (this.#stopped) return;
        await this.#pause("长连接断了");
      } catch (error) {
        if (this.#stopped) return;
        await this.#pause("长连接出错", error);
      }
    }
  }

  /**
   * 退避一会儿再重连。
   *
   * 这里**不是一觉睡到底**，而是每 200 毫秒醒一下看有没有被叫停 —— 不然按
   * `Ctrl-C` 之后还得干等十几秒才退得出去。（跟 Discord 那家同一个写法。）
   */
  async #pause(reason: string, error?: unknown): Promise<void> {
    this.#failures += 1;
    const waitMs = Math.min(MAX_PAUSE_MS, 1000 * 2 ** Math.min(this.#failures, 5));
    const detail = error === undefined ? "" : `：${explain(error)}`;
    this.#log(
      `${reason}（第 ${this.#failures} 次），${Math.round(waitMs / 1000)} 秒后重连${detail}`,
    );

    const until = Date.now() + waitMs;
    while (!this.#stopped && Date.now() < until) {
      await delay(Math.min(200, until - Date.now()));
    }
  }

  stop(): void {
    this.#stopped = true;
    this.#socket?.close();
    this.#socket = undefined;
  }

  // ---- 换地址、连上、然后一直读 ------------------------------------------

  /**
   * 一次完整的连接：换地址 → 连上 → 读帧，直到它断或者被叫停。
   *
   * 换地址那步失败会**抛出去**（重连从 `run` 那边算）；连上之后的断开是正常
   * 返回，因为那是长连接本来就该有的一部分。
   */
  async #connect(): Promise<void> {
    const endpoint = await this.#handshake();
    this.#service = endpoint.service;
    this.#warnedFrame = false;

    const socket = this.#open(endpoint.url);
    this.#socket = socket;

    await new Promise<void>((resolve, reject) => {
      let pingMs = endpoint.pingMs;
      let heartbeat: ReturnType<typeof setInterval> | undefined;
      let watchdog: ReturnType<typeof setInterval> | undefined;
      let lastHeard = Date.now();
      let settled = false;

      const finish = (error?: unknown): void => {
        if (settled) return;
        settled = true;
        if (heartbeat !== undefined) clearInterval(heartbeat);
        if (watchdog !== undefined) clearInterval(watchdog);
        if (this.#socket === socket) this.#socket = undefined;
        socket.close();
        if (error === undefined) resolve();
        else reject(error);
      };

      const beat = (): void => {
        if (settled) return;
        this.#sweep();
        this.#send(socket, buildPing(this.#service, this.#nextSeq()));
      };

      const restartHeartbeat = (interval: number): void => {
        pingMs = interval;
        if (heartbeat !== undefined) clearInterval(heartbeat);
        heartbeat = setInterval(beat, pingMs);
      };

      socket.onopen = () => {
        // 第一帧必须由我们发出去：服务端不会先开口，所以得等它真的连上。
        // 标准 WebSocket 的 `onopen` 是异步来的，抢在它之前 send 会直接抛。
        restartHeartbeat(pingMs);
        beat();
        watchdog = setInterval(() => {
          if (settled) return;
          const limit = idleTimeoutMs(pingMs);
          if (Date.now() - lastHeard < limit) return;
          finish(new Error(`${Math.round(limit / 1000)} 秒没收到任何东西，当它已经死了`));
        }, 10_000);
      };

      socket.onmessage = (event) => {
        if (settled) return;
        const bytes = toBytes(event.data);
        if (bytes === null) {
          if (!this.#warnedFrame) {
            this.#warnedFrame = true;
            this.#log(
              event.data instanceof Blob
                ? "帧是 Blob 递过来的 —— 这根连接没把 binaryType 设成 arraybuffer，帧全丢了"
                : "收到一个不是二进制的帧，丢掉",
            );
          }
          return;
        }
        lastHeard = Date.now();

        const frame = decodeFrame(bytes);
        if (frame === null) {
          this.#log("认不出一帧，丢掉");
          return;
        }

        if (frame.method === METHOD_CONTROL) {
          const updated = pingIntervalOf(frame);
          if (updated !== null && updated !== pingMs) {
            this.#log(`服务端把心跳间隔调成了 ${Math.round(updated / 1000)} 秒`);
            restartHeartbeat(updated);
          }
          return;
        }
        if (frame.method !== METHOD_DATA) {
          this.#log(`收到一个不认识的帧（method=${frame.method}），丢掉`);
          return;
        }

        // **先把 ACK 拍回去**，再去干别的。飞书要求 3 秒内应答，而「想清楚再
        // 回答」可能要好几个来回 —— 应答和答复在这里是两件事，这也是为什么
        // 这里没有微信那种竞速。
        this.#ack(socket, frame);

        void this.#onData(frame).catch((error: unknown) => {
          this.#log(`处理这一帧时出错：${explain(error)}`);
        });
      };

      socket.onclose = () => finish();
      socket.onerror = () => finish(new Error("长连接的 socket 报错了"));
    });
  }

  #nextSeq(): bigint {
    this.#seq += 1n;
    return this.#seq;
  }

  #send(socket: LarkSocket, frame: LarkFrame): void {
    try {
      socket.send(encodeFrame(frame));
    } catch (error) {
      // 发不出去不该把整个网关带走 —— 下一轮心跳还会再试，再不行看门狗会把
      // 它判死然后重连。
      this.#log(`一帧没发出去：${explain(error)}`);
    }
  }

  #ack(socket: LarkSocket, frame: LarkFrame): void {
    this.#send(socket, buildAck(frame));
  }

  /**
   * 拿 AppID/AppSecret 换一个带票据的连接地址。
   *
   * 注意换来的地址里有 `service_id`，**之后每一帧都要带**。
   */
  async #handshake(): Promise<{ url: string; service: number; pingMs: number }> {
    const response = await this.#fetch(`${this.#wsBase}/callback/ws/endpoint`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        // 官方 SDK 都会带这个。不带多半也能拿到地址，但那不是设想的用法。
        locale: "zh",
      },
      body: JSON.stringify({ AppID: this.#appId, AppSecret: this.#appSecret }),
    });

    if (!response.ok) {
      throw new Error(`换连接地址失败：HTTP ${response.status} ${await response.text()}`);
    }

    const envelope = (await response.json()) as {
      code?: number;
      msg?: string;
      data?: { URL?: string; ClientConfig?: { PingInterval?: number } };
    };
    const url = envelope.data?.URL ?? "";
    if (envelope.code !== 0 || url === "") {
      throw new Error(
        `换连接地址被拒：code=${String(envelope.code)} ${envelope.msg ?? ""} —— 多半是 ` +
          "AppID/AppSecret 不对，或者这个应用没开长连接",
      );
    }

    // 握手给的间隔跟 pong 里给的是同一套规矩：小于 10 秒当没说。
    const seconds = envelope.data?.ClientConfig?.PingInterval ?? 0;
    const pingMs = seconds >= MIN_PING_MS / 1000 ? seconds * 1000 : DEFAULT_PING_MS;

    return { url, service: serviceIdOf(url), pingMs };
  }

  // ---- 帧 → 事件 ----------------------------------------------------------

  /** 数据帧处理完一轮。ACK 已经先发过了，所以这里出错不会让飞书重发。 */
  async #onData(frame: LarkFrame): Promise<void> {
    const type = headerValue(frame, "type");
    if (type !== "event") {
      // "card" 是交互卡片的回调。留句话，不然以后有人对着日志猜。
      if (type !== "") this.#log(`收到 ${type} 帧，还没接`);
      return;
    }

    const messageId = headerValue(frame, "message_id");
    const payload = this.#reassemble(frame, messageId);
    if (payload === null) return; // 分片还差几片

    if (messageId !== "" && !this.#claim(messageId)) {
      this.#log("这一条刚才处理过了（飞书重发），跳过");
      return;
    }

    let event: LarkEvent;
    try {
      event = JSON.parse(new TextDecoder().decode(payload)) as LarkEvent;
    } catch {
      this.#log("事件不是 JSON，丢掉");
      return;
    }

    await this.#dispatch(event);
  }

  /**
   * 把分片拼回一整段。没分片的话原样返回。
   *
   * 返回 `null` 是「还差几片，别当它处理完了」—— 这一点要紧：把半截 JSON 拿去
   * 解析，只会得到一句「事件不是 JSON，丢掉」，然后你永远不知道真实原因是一条
   * 大消息。
   */
  #reassemble(frame: LarkFrame, messageId: string): Uint8Array | null {
    const payload = frame.payload ?? new Uint8Array(0);
    const sum = headerInt(frame, "sum");
    const seq = headerInt(frame, "seq");

    if (sum <= 1) return payload;

    if (messageId === "" || seq < 0 || seq >= sum) {
      // 说了要分片，却没给分组键、或者序号越界 —— 这帧是坏的。当成整条会拿到
      // 半截 JSON，不如直接扔掉。
      this.#log(`分片帧不对劲（sum=${sum} seq=${seq}），丢掉`);
      return null;
    }

    // `new Array(sum)` 建出来的是**稀疏**数组，空洞会被 `some`/`reduce` 跳过 ——
    // 那样拿到第一片就以为凑齐了，正好掉进上面说的那个坑。必须 `fill` 成实心的。
    const blank = (): Array<Uint8Array | undefined> =>
      new Array<Uint8Array | undefined>(sum).fill(undefined);

    const entry = this.#fragments.get(messageId) ?? { parts: blank(), at: 0 };
    if (entry.parts.length !== sum) entry.parts = blank();
    entry.parts[seq] = payload;
    entry.at = Date.now();
    this.#fragments.set(messageId, entry);

    if (entry.parts.some((part) => part === undefined)) return null;

    this.#fragments.delete(messageId);
    return concat(entry.parts as Uint8Array[]);
  }

  /** 第一次见这个 `message_id` 就返回 `true`，并记下它。 */
  #claim(messageId: string): boolean {
    const at = this.#seen.get(messageId);
    if (at !== undefined && Date.now() - at < SEEN_TTL_MS) return false;
    this.#seen.set(messageId, Date.now());
    return true;
  }

  /** 顺手把过期的记性清掉。每次心跳跑一回。 */
  #sweep(): void {
    const now = Date.now();
    for (const [key, at] of this.#seen) {
      if (now - at >= SEEN_TTL_MS) this.#seen.delete(key);
    }
    for (const [key, entry] of this.#fragments) {
      if (now - entry.at >= FRAGMENT_TTL_MS) this.#fragments.delete(key);
    }
  }

  // ---- 事件 → 一句话 ------------------------------------------------------

  async #dispatch(event: LarkEvent): Promise<void> {
    if (event.header?.event_type !== MESSAGE_EVENT) return;

    const message = event.event?.message;
    const sender = event.event?.sender;
    if (message === undefined || sender === undefined) return;

    // 机器人自己发的消息也会从这条连接回来（比如刚发出去的那条答复）。
    // 不挡住就是自问自答。
    if (sender.sender_type === "app" || sender.sender_type === "bot") return;

    const openId = sender.sender_id?.open_id ?? "";
    const chatId = message.chat_id ?? "";
    if (openId === "" || chatId === "") {
      this.#log("事件里没有 open_id 或 chat_id，丢掉");
      return;
    }

    if (this.#allow.size > 0 && !this.#allow.has(openId)) {
      this.#log(`不在白名单里，不理：${openId}`);
      return;
    }

    // 群里不叫它就别接话 —— 跟 Discord 那条同一个理由：一个对每句话都插嘴的
    // 机器人，第二天就会被移出群。单聊里没必要，那儿本来就只有你们俩。
    const inGroup = (message.chat_type ?? "p2p") !== "p2p";
    if (inGroup && !(await this.#mentionsMe(message.mentions ?? []))) return;

    const text =
      message.message_type === "text" ? stripMentionKeys(this.#textOf(message.content ?? "")) : "";

    if (text === "") {
      await this.#say(chatId, "这条我读不了 —— 目前只认文字，图片、语音和文件还没接。");
      return;
    }

    const conversation = `lark:${chatId}`;
    this.#log(`${conversation}：「${text}」`);

    let reply: string;
    try {
      reply = await this.#respond(conversation, text);
    } catch (error) {
      reply = `我这边出错了：${explain(error)}`;
    }

    await this.#say(chatId, reply);
  }

  /** 文本消息的 `content` 是 `{"text":"..."}` 这样一段 JSON。 */
  #textOf(content: string): string {
    if (content === "") return "";
    try {
      const parsed = JSON.parse(content) as { text?: unknown };
      return typeof parsed.text === "string" ? parsed.text : "";
    } catch {
      return "";
    }
  }

  /**
   * 群里这条是不是在叫它。
   *
   * 得先知道自己是谁：飞书的 `mentions` 是**这条消息里所有的 @**，@ 到同事也算。
   * 所以要拿机器人的 `open_id` 去比。
   *
   * 拿不到的话退回到「有 @ 就算」—— 会多嘴，但不会漏掉叫你。这条退路上的日志
   * 得说清楚，不然多嘴这事没人查得动。
   */
  async #mentionsMe(mentions: readonly LarkMention[]): Promise<boolean> {
    if (mentions.length === 0) return false;
    const self = await this.#botOpenId();
    if (self === undefined) {
      if (!this.#warnedSelf) {
        this.#warnedSelf = true;
        this.#log("问不到机器人自己的 open_id —— 群里只能按「有 @ 就算」，@ 到别人也可能接话");
      }
      return true;
    }
    return mentions.some((mention) => mention.id?.open_id === self);
  }

  /**
   * 机器人自己的 `open_id`。问不到就返回 `undefined`（只问一次，不再试）。
   *
   * 响应形状（核实过，两处一致）：
   *
   * ```json
   * { "code": 0, "msg": "ok",
   *   "bot": { "activate_status": 2, "app_name": "…", "avatar_url": "…",
   *            "ip_white_list": [], "open_id": "ou_…" } }
   * ```
   *
   * 两个要紧的点：`bot` 在**顶层**，不是套在 `data` 里；要的字段就叫 `open_id`
   * （跟 APP_ID 的 `cli_…` 完全是两码事）。来源：官方文档的响应体表，和官方
   * Go SDK 的生成代码 `BotGetResult{Bot *BotInfo `json:"bot"`}` —— 两边对得上。
   *
   * 这个接口要求应用**已开启机器人能力并发布**。没开的话它回一个非 0 的 `code`：
   * 有响应、但没有 `bot`。这种情况必须单独记一笔 —— 不然退化成宽松规则之后，
   * 你只看到它在群里乱接话，却查不到是因为机器人能力没开。
   */
  async #botOpenId(): Promise<string | undefined> {
    if (this.#selfId !== null) return this.#selfId;

    try {
      const token = await this.#accessToken();
      const response = await this.#fetch(`${this.#apiBase}/bot/v3/info`, {
        headers: { authorization: `Bearer ${token}` },
      });
      const body = (await response.json()) as {
        code?: number;
        msg?: string;
        bot?: { open_id?: unknown };
      };

      if (body.code !== 0) {
        this.#selfId = undefined;
        this.#log(
          `问机器人信息被拒：code=${String(body.code)} ${body.msg ?? ""} —— ` +
            "多半是这个应用没开机器人能力、或者没发版",
        );
        return this.#selfId;
      }

      const openId = body.bot?.open_id;
      if (typeof openId !== "string" || openId === "") {
        this.#selfId = undefined;
        this.#log("机器人信息里没有 open_id，问不出来");
        return this.#selfId;
      }

      this.#selfId = openId;
    } catch (error) {
      this.#selfId = undefined;
      this.#log(`问机器人信息失败：${explain(error)}`);
    }

    return this.#selfId;
  }

  // ---- 发出去 --------------------------------------------------------------

  /**
   * 拿 `tenant_access_token`。缓存到快过期为止。
   *
   * 提前 2 分钟作废，免得卡在「刚取到就过期」那条缝上。
   */
  async #accessToken(): Promise<string> {
    if (this.#token !== undefined && Date.now() < this.#token.until) return this.#token.value;

    const response = await this.#fetch(`${this.#apiBase}/auth/v3/tenant_access_token/internal`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ app_id: this.#appId, app_secret: this.#appSecret }),
    });
    const body = (await response.json()) as {
      code?: number;
      msg?: string;
      tenant_access_token?: string;
      expire?: number;
    };

    const value = body.tenant_access_token ?? "";
    if (body.code !== 0 || value === "") {
      throw new Error(`拿 tenant_access_token 失败：code=${String(body.code)} ${body.msg ?? ""}`);
    }

    const life = Math.max(60_000, (body.expire ?? 7200) * 1000 - 120_000);
    this.#token = { value, until: Date.now() + life };
    return value;
  }

  /** 一条太长的答复切成几条发。切法跟另外三家共用。 */
  async #say(chatId: string, text: string): Promise<void> {
    for (const chunk of splitMessage(text, MESSAGE_LIMIT)) {
      try {
        await this.#sendText(chatId, chunk);
      } catch (error) {
        // 发不出去不该把这条连接带走 —— 用户没看到答复，但网关还得接着干活。
        this.#log(`发消息失败：${explain(error)}`);
        return;
      }
    }
  }

  async #sendText(chatId: string, text: string): Promise<void> {
    // 令牌失效会返回 HTTP 200 + 一个非零 code，所以不能只看 response.ok。
    // 99991663 / 99991661 是「令牌不对/过期」—— 撞上了就扔掉缓存重来一次。
    for (let attempt = 0; ; attempt += 1) {
      const token = await this.#accessToken();
      const response = await this.#fetch(`${this.#apiBase}/im/v1/messages?receive_id_type=chat_id`, {
        method: "POST",
        headers: {
          "content-type": "application/json; charset=utf-8",
          authorization: `Bearer ${token}`,
        },
        body: JSON.stringify({
          receive_id: chatId,
          msg_type: "text",
          content: JSON.stringify({ text }),
        }),
      });
      const body = (await response.json()) as { code?: number; msg?: string };

      if (body.code === 0) return;

      const stale = body.code === 99_991_663 || body.code === 99_991_661;
      if (stale && attempt === 0) {
        this.#token = undefined;
        continue;
      }
      throw new Error(`发消息失败：code=${String(body.code)} ${body.msg ?? ""}`);
    }
  }
}

/**
 * 真的那根 WebSocket。
 *
 * `binaryType` 是这里唯一不能省的一行：默认值是 `blob`，而 Blob 只能异步读 ——
 * 按默认值走就是「连上了、一帧都读不出来」。
 */
function defaultSocketFactory(url: string): LarkSocket {
  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";
  return socket as unknown as LarkSocket;
}

