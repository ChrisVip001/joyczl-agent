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
 * The Lark (Feishu) adapter — **long connection**, no public address needed.
 *
 * The most awkward protocol of the four, awkward in two ways:
 *
 * 1. **Frames aren't JSON but Lark's private protobuf** (`pbbp2`). Hence the neighboring
 *    `lark-proto.ts`: a handwritten codec for this one message. The official route is pulling in
 *    an SDK, but this project hasn't pulled in a platform SDK since Discord — a whole protobuf
 *    runtime for one message costs more than it buys.
 * 2. **Fetch the address before connecting**. Not just opening a fixed wss URL: trade
 *    AppID/AppSecret for an address **carrying a one-time ticket**, then connect to that. The
 *    address contains a `service_id` that every later frame must carry.
 *
 * A contrast with WeChat, exactly reversed: **Lark wrote "ack first, answer later" into the
 * rules** (a data frame must be ACKed within 3 seconds; the reply goes out over HTTP). WeChat
 * made us time a 4-second watch ourselves. So no race here — ACK immediately, then think.
 */

/**
 * Character cap for one message.
 *
 * Lark's text-message request body caps at **150KB**, erroring `230025` past it. 30k chars is
 * ~90KB in Chinese, with headroom. This is a "bounded" choice, not a wall actually hit — the
 * official send-message page never states the number.
 */
export const MESSAGE_LIMIT = 30_000;

/** Lark, and international Lark. The two domains share API paths; only the host differs. */
const DOMAINS = {
  feishu: { api: "https://open.feishu.cn/open-apis", ws: "https://open.feishu.cn" },
  lark: { api: "https://open.larksuite.com/open-apis", ws: "https://open.larksuite.com" },
} as const;

/** Default heartbeat interval when the server doesn't send one. */
const DEFAULT_PING_MS = 120_000;

/**
 * Lower bound for the heartbeat interval. **This one is documented**: the official SDK says
 * intervals under 10 seconds are to be "silently ignored" — keep the default. The server counts
 * heartbeat rounds by its own interval; beating more often only scrambles the rounds.
 */
const MIN_PING_MS = 10_000;

/** The ACK payload. Fixed shape; both third-party implementations send exactly this. */
const ACK_PAYLOAD = new TextEncoder().encode('{"code":200,"headers":{},"data":[]}');

/** How long to remember `message_id`s for dedup. Lark's re-sends never wait half an hour. */
const SEEN_TTL_MS = 30 * 60 * 1000;

/** How long to keep fragment stubs. Expired ones are dropped — a missing piece never completes. */
const FRAGMENT_TTL_MS = 5 * 60 * 1000;

/** Reconnect backoff cap. Same number as the Discord adapter. */
const MAX_PAUSE_MS = 30_000;

/**
 * How long with nothing received before this connection is declared dead.
 *
 * A half-open TCP connection (closed laptop, NAT timeout) doesn't error; it just quietly stops
 * delivering — the nastiest failure: the process lives on, users think it's alive. Both
 * third-party implementations use 300s; here it follows the heartbeat interval, still holding if raised.
 */
function idleTimeoutMs(pingMs: number): number {
  return Math.max(300_000, pingMs * 2 + 60_000);
}

/**
 * Extract `service_id` from the handshake URL.
 *
 * Field 3 of every later frame must carry it; a wrong value makes the server act deaf — another
 * silent failure. Hence a separate function, separately tested.
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

/** Heartbeat frame: a control frame carrying only `type: ping`. `seqId` is our own counter. */
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
 * The ACK frame: **echo the received frame whole**, changing only two things — the payload
 * becomes the fixed JSON string, and a `biz_rt` header is added.
 *
 * "Echo" rather than "build a new frame": the frame's `seqId` and `logId` are how the server
 * matches this reply to its request; inventing new numbers breaks the match. Even header order stays.
 *
 * **Don't mutate the frame passed in** — it's still needed to read the event; mutating it in
 * place causes failures far from the scene.
 */
export function buildAck(frame: LarkFrame): LarkFrame {
  const headers: LarkFrameHeader[] = [...frame.headers];
  if (!headers.some((header) => header.key === "biz_rt")) {
    headers.push({ key: "biz_rt", value: "0" });
  }
  return { ...frame, headers, payload: ACK_PAYLOAD };
}

/** Only this one event type is accepted. Others (card callbacks, joins, recalls) aren't wired yet. */
const MESSAGE_EVENT = "im.message.receive_v1";

/**
 * Strip the `@_user_1` placeholders out of the text.
 *
 * Lark puts placeholders in `content.text`, with real names in `mentions`. Placeholders are
 * noise to the model — remove them directly; both third-party implementations do the same.
 */
export function stripMentionKeys(text: string): string {
  return text
    .replace(/@_user_\d+/g, "")
    .replace(/\s{2,}/g, " ")
    .trim();
}

/**
 * A pong may carry a new heartbeat interval (the server tunes it itself).
 *
 * Returning `null` means "not stated this time" — not the same as "stated 0 seconds"; keep them apart.
 *
 * **Anything under 10 seconds counts as unstated** (not clamped to 10): the official SDK's
 * answer is "silently ignore, keep the default". The server counts its rounds by that number;
 * clamping would scramble them — politely treating it as unmentioned beats picking a number.
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

/** Read an integer from one header. Empty strings and garbage both count as 0. */
function headerInt(frame: LarkFrame, key: string): number {
  const raw = headerValue(frame, key);
  if (raw === "") return 0;
  const value = Number(raw);
  return Number.isSafeInteger(value) ? value : 0;
}

/** Collect a frame's `data` into bytes. Not binary: `null`. */
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

// ---- The socket we connect with ---------------------------------------------------

/**
 * The minimal shape of a connection. Like Discord's, it exists so tests can slot a fake in —
 * just the WebSocket callbacks plus two methods.
 */
export interface LarkSocket {
  send(data: string | Uint8Array): void;
  close(): void;
  onopen: (() => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
  /**
   * Binary frames must arrive as `ArrayBuffer`.
   *
   * Not optional: WHATWG's WebSocket defaults to **Blob**, readable only asynchronously. With
   * the default you get "connected, yet not one frame decodes" — another silent failure. The
   * default factory sets `arraybuffer`, and a stray Blob is called out loudly.
   */
  binaryType?: string;
}

export type LarkSocketFactory = (url: string) => LarkSocket;

export interface LarkAdapterOptions {
  appId: string;
  appSecret: string;
  /** Allowlist: only these `open_id`s get answered. Unset = answer everyone. */
  allow?: readonly string[];
  /** Connect to international Lark (`open.larksuite.com`) instead of Feishu. */
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

  /** When an undecodable frame arrives, complain once per connection, not every frame. */
  #warnedFrame = false;

  /** Processed `message_id`s → time processed. Lark re-sends are skipped via this. */
  readonly #seen = new Map<string, number>();

  /** Half-assembled fragments. `message_id` → the pieces. */
  readonly #fragments = new Map<string, { parts: Array<Uint8Array | undefined>; at: number }>();

  /** `tenant_access_token` lasts 2 hours; cache it instead of asking per message. */
  #token: { value: string; until: number } | undefined;

  /** The bot's own `open_id`, to tell whether a group message calls it. `null` = not asked yet. */
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

  /** Keep connecting until stopped. Reconnect on drops with doubled backoff, capped at 30s. */
  async run(): Promise<void> {
    while (!this.#stopped) {
      try {
        await this.#connect();
        // A connection reached clears the failure count: one drop shouldn't lengthen the next wait.
        this.#failures = 0;
        if (this.#stopped) return;
        await this.#pause("the long connection dropped");
      } catch (error) {
        if (this.#stopped) return;
        await this.#pause("the long connection failed", error);
      }
    }
  }

  /**
   * Back off a while, then reconnect.
   *
   * This is **not one long sleep** — wake every 200ms to check for a stop, otherwise Ctrl-C
   * would sit out a dozen seconds before exiting. (Same pattern as the Discord adapter.)
   */
  async #pause(reason: string, error?: unknown): Promise<void> {
    this.#failures += 1;
    const waitMs = Math.min(MAX_PAUSE_MS, 1000 * 2 ** Math.min(this.#failures, 5));
    const detail = error === undefined ? "" : `：${explain(error)}`;
    this.#log(
      `${reason} (failure ${this.#failures}); reconnecting in ${Math.round(waitMs / 1000)} s${detail}`,
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

  // ---- Trade for the address, connect, then read forever ------------------------------------------

  /**
   * One full connection: trade for the address → connect → read frames, until it drops or is stopped.
   *
   * A failure in the address step **throws** (reconnects are counted from `run`'s side); a drop
   * after connecting returns normally, since that's a normal part of a long connection's life.
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
        // The first frame must be ours: the server never speaks first, so wait for a real connect.
        // The standard WebSocket's `onopen` arrives async — sending before it throws outright.
        restartHeartbeat(pingMs);
        beat();
        watchdog = setInterval(() => {
          if (settled) return;
          const limit = idleTimeoutMs(pingMs);
          if (Date.now() - lastHeard < limit) return;
          finish(new Error(`nothing arrived for ${Math.round(limit / 1000)} s; treating it as dead`));
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
                ? "frames arrived as Blobs — this connection never set binaryType to arraybuffer, so every frame is lost"
                : "a frame that is not binary arrived; dropped",
            );
          }
          return;
        }
        lastHeard = Date.now();

        const frame = decodeFrame(bytes);
        if (frame === null) {
          this.#log("a frame could not be recognised; dropped");
          return;
        }

        if (frame.method === METHOD_CONTROL) {
          const updated = pingIntervalOf(frame);
          if (updated !== null && updated !== pingMs) {
            this.#log(`the server moved the heartbeat interval to ${Math.round(updated / 1000)} s`);
            restartHeartbeat(updated);
          }
          return;
        }
        if (frame.method !== METHOD_DATA) {
          this.#log(`an unknown frame arrived (method=${frame.method}); dropped`);
          return;
        }

        // **Slam the ACK back first**, then do anything else. Lark requires a reply within 3
        // seconds, while "thinking it over" may take several round trips — acknowledgment and
        // answer are two things here, which is why there's no WeChat-style race.
        this.#ack(socket, frame);

        void this.#onData(frame).catch((error: unknown) => {
          this.#log(`handling this frame failed: ${explain(error)}`);
        });
      };

      socket.onclose = () => finish();
      socket.onerror = () => finish(new Error("the long connection socket reported an error"));
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
      // A failed send must not take the gateway down — the next heartbeat retries, and the
      // watchdog declares death and reconnects if it keeps failing.
      this.#log(`a frame did not go out: ${explain(error)}`);
    }
  }

  #ack(socket: LarkSocket, frame: LarkFrame): void {
    this.#send(socket, buildAck(frame));
  }

  /**
   * Trade AppID/AppSecret for a ticketed connection address.
   *
   * Note the address carries a `service_id` that **every later frame must include**.
   */
  async #handshake(): Promise<{ url: string; service: number; pingMs: number }> {
    const response = await this.#fetch(`${this.#wsBase}/callback/ws/endpoint`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        // The official SDKs all send this. It probably works without, but that's not the intended use.
        locale: "zh",
      },
      body: JSON.stringify({ AppID: this.#appId, AppSecret: this.#appSecret }),
    });

    if (!response.ok) {
      throw new Error(`could not exchange the connection address: HTTP ${response.status} ${await response.text()}`);
    }

    const envelope = (await response.json()) as {
      code?: number;
      msg?: string;
      data?: { URL?: string; ClientConfig?: { PingInterval?: number } };
    };
    const url = envelope.data?.URL ?? "";
    if (envelope.code !== 0 || url === "") {
      throw new Error(
        `the connection address was refused: code=${String(envelope.code)} ${envelope.msg ?? ""} — most likely ` +
          "the AppID/AppSecret is wrong, or the app has no long connection enabled",
      );
    }

    // The handshake interval follows the same rule as the pong's: under 10 seconds counts as unstated.
    const seconds = envelope.data?.ClientConfig?.PingInterval ?? 0;
    const pingMs = seconds >= MIN_PING_MS / 1000 ? seconds * 1000 : DEFAULT_PING_MS;

    return { url, service: serviceIdOf(url), pingMs };
  }

  // ---- Frames → events ----------------------------------------------------------

  /** One data frame processed. The ACK already went out, so an error here won't trigger a re-send. */
  async #onData(frame: LarkFrame): Promise<void> {
    const type = headerValue(frame, "type");
    if (type !== "event") {
      // "card" is an interactive-card callback. Leave a line, or someone will be guessing from logs.
      if (type !== "") this.#log(`a ${type} frame arrived; not wired up yet`);
      return;
    }

    const messageId = headerValue(frame, "message_id");
    const payload = this.#reassemble(frame, messageId);
    if (payload === null) return; // fragments still incomplete

    if (messageId !== "" && !this.#claim(messageId)) {
      this.#log("this one was handled a moment ago (Feishu resent it); skipped");
      return;
    }

    let event: LarkEvent;
    try {
      event = JSON.parse(new TextDecoder().decode(payload)) as LarkEvent;
    } catch {
      this.#log("the event is not JSON; dropped");
      return;
    }

    await this.#dispatch(event);
  }

  /**
   * Reassemble fragments into one payload. Unfragmented: returned as-is.
   *
   * `null` means "pieces still missing, don't treat it as done" — this matters: parsing half a
   * JSON only yields "event isn't JSON, dropped", and you'd never learn the real cause was one
   * large message.
   */
  #reassemble(frame: LarkFrame, messageId: string): Uint8Array | null {
    const payload = frame.payload ?? new Uint8Array(0);
    const sum = headerInt(frame, "sum");
    const seq = headerInt(frame, "seq");

    if (sum <= 1) return payload;

    if (messageId === "" || seq < 0 || seq >= sum) {
      // It declared fragmentation but gave no group key, or the index is out of range — the frame
      // is broken. Treating it as whole would yield half a JSON; dropping it outright is better.
      this.#log(`the fragmented frame does not add up (sum=${sum} seq=${seq}); dropped`);
      return null;
    }

    // `new Array(sum)` builds a **sparse** array; holes get skipped by `some`/`reduce` — you'd
    // think one piece completes it, falling into the trap above. It must be `fill`ed solid.
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

  /** Returns `true` the first time this `message_id` is seen, and records it. */
  #claim(messageId: string): boolean {
    const at = this.#seen.get(messageId);
    if (at !== undefined && Date.now() - at < SEEN_TTL_MS) return false;
    this.#seen.set(messageId, Date.now());
    return true;
  }

  /** Sweep expired memories while at it. Runs once per heartbeat. */
  #sweep(): void {
    const now = Date.now();
    for (const [key, at] of this.#seen) {
      if (now - at >= SEEN_TTL_MS) this.#seen.delete(key);
    }
    for (const [key, entry] of this.#fragments) {
      if (now - entry.at >= FRAGMENT_TTL_MS) this.#fragments.delete(key);
    }
  }

  // ---- Events → one reply ------------------------------------------------------

  async #dispatch(event: LarkEvent): Promise<void> {
    if (event.header?.event_type !== MESSAGE_EVENT) return;

    const message = event.event?.message;
    const sender = event.event?.sender;
    if (message === undefined || sender === undefined) return;

    // Messages the bot itself sent also come back over this connection (like the reply just sent).
    // Unblocked, it would answer itself.
    if (sender.sender_type === "app" || sender.sender_type === "bot") return;

    const openId = sender.sender_id?.open_id ?? "";
    const chatId = message.chat_id ?? "";
    if (openId === "" || chatId === "") {
      this.#log("the event carries neither open_id nor chat_id; dropped");
      return;
    }

    if (this.#allow.size > 0 && !this.#allow.has(openId)) {
      this.#log(`outside the allowlist, not answering: ${openId}`);
      return;
    }

    // In groups, stay silent unless called — same reason as Discord: a bot that chimes in on
    // everything gets removed from the group the next day. DMs don't need it; it's just you two.
    const inGroup = (message.chat_type ?? "p2p") !== "p2p";
    if (inGroup && !(await this.#mentionsMe(message.mentions ?? []))) return;

    const text =
      message.message_type === "text" ? stripMentionKeys(this.#textOf(message.content ?? "")) : "";

    if (text === "") {
      await this.#say(chatId, "I cannot read this one — text only for now; images, voice and files are not wired up yet.");
      return;
    }

    const conversation = `lark:${chatId}`;
    this.#log(`${conversation}：「${text}」`);

    let reply: string;
    try {
      reply = await this.#respond(conversation, text);
    } catch (error) {
      reply = `something failed on my side: ${explain(error)}`;
    }

    await this.#say(chatId, reply);
  }

  /** A text message's `content` is a JSON blob like `{"text":"..."}`. */
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
   * Is this group message calling the bot?
   *
   * First we must know who we are: Lark's `mentions` holds **every @ in the message**, including
   * ones @-ing colleagues. So compare against the bot's `open_id`.
   *
   * If it can't be fetched, fall back to "any @ counts" — chattier, but it never misses a call.
   * The log on that fallback path must say so clearly, or nobody could ever debug the chattiness.
   */
  async #mentionsMe(mentions: readonly LarkMention[]): Promise<boolean> {
    if (mentions.length === 0) return false;
    const self = await this.#botOpenId();
    if (self === undefined) {
      if (!this.#warnedSelf) {
        this.#warnedSelf = true;
        this.#log("cannot ask for the bot's own open_id — in a group it has to go by «an @ means me», so it may answer an @ aimed at someone else");
      }
      return true;
    }
    return mentions.some((mention) => mention.id?.open_id === self);
  }

  /**
   * The bot's own `open_id`. Returns `undefined` when unanswerable (asked once, never retried).
   *
   * Response shape (verified, two sources agree):
   *
   * ```json
   * { "code": 0, "msg": "ok",
   *   "bot": { "activate_status": 2, "app_name": "…", "avatar_url": "…",
   *            "ip_white_list": [], "open_id": "ou_…" } }
   * ```
   *
   * Two crucial points: `bot` sits at the **top level**, not inside `data`; the field is
   * literally named `open_id` (nothing like the APP_ID's `cli_…`). Sources: the official docs'
   * response table and the Go SDK's generated `BotGetResult{Bot *BotInfo `json:"bot"`}` — they agree.
   *
   * This API requires the app to **have bot capability enabled and be published**. Otherwise it
   * returns a non-zero `code`: a response, but no `bot`. Log that case separately — once we
   * degrade to the loose rule, the trace that bot capability was off is nowhere to be found.
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
          `asking for the bot's own info was refused: code=${String(body.code)} ${body.msg ?? ""} — ` +
            "most likely the app has no bot capability enabled, or was never released",
        );
        return this.#selfId;
      }

      const openId = body.bot?.open_id;
      if (typeof openId !== "string" || openId === "") {
        this.#selfId = undefined;
        this.#log("the bot info carries no open_id, so it cannot be asked");
        return this.#selfId;
      }

      this.#selfId = openId;
    } catch (error) {
      this.#selfId = undefined;
      this.#log(`asking for the bot's own info failed: ${explain(error)}`);
    }

    return this.#selfId;
  }

  // ---- Sending out --------------------------------------------------------------

  /**
   * Fetch `tenant_access_token`. Cached until near expiry.
   *
   * Invalidated 2 minutes early, so we don't land in the "expired right after fetching" crack.
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
      throw new Error(`could not get a tenant_access_token: code=${String(body.code)} ${body.msg ?? ""}`);
    }

    const life = Math.max(60_000, (body.expire ?? 7200) * 1000 - 120_000);
    this.#token = { value, until: Date.now() + life };
    return value;
  }

  /** Split an overlong reply into several messages. Same splitting as the other three. */
  async #say(chatId: string, text: string): Promise<void> {
    for (const chunk of splitMessage(text, MESSAGE_LIMIT)) {
      try {
        await this.#sendText(chatId, chunk);
      } catch (error) {
        // A failed send must not take the connection down — the gateway must keep serving.
        this.#log(`sending the message failed: ${explain(error)}`);
        return;
      }
    }
  }

  async #sendText(chatId: string, text: string): Promise<void> {
    // A stale token returns HTTP 200 with a non-zero code, so response.ok alone isn't enough.
    // 99991663 / 99991661 mean "token wrong/expired" — drop the cache and retry once on a hit.
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
      throw new Error(`sending the message failed: code=${String(body.code)} ${body.msg ?? ""}`);
    }
  }
}

/**
 * The real WebSocket.
 *
 * `binaryType` is the one line that can't be skipped here: the default is `blob`, and a Blob is
 * only readable asynchronously — with the default you get "connected, not one frame decodes".
 */
function defaultSocketFactory(url: string): LarkSocket {
  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";
  return socket as unknown as LarkSocket;
}

