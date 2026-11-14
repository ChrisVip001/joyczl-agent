import { explain } from "./bridge.ts";
import type { Responder } from "./bridge.ts";
import { splitMessage } from "./text.ts";

/** Discord's per-message cap. Past it the API returns 400; it won't truncate for you. */
export const MESSAGE_LIMIT = 2000;

/**
 * The gateway URL. `encoding=json` is the only encoding worth supporting — the other (etf) is
 * for libraries handling hundreds of thousands of events a day.
 *
 * WebSocket here rather than webhook: a webhook needs a public HTTPS address, a certificate, and
 * Discord's signature verification plus the "reply 200 first, process later" dance. Connecting
 * out just works — the same reason Telegram picked long polling.
 */
const GATEWAY_URL = "wss://gateway.discord.gg/?v=10&encoding=json";
const API_URL = "https://discord.com/api/v10";

/**
 * Discord requires every request to identify itself, as `DiscordBot (url, version)`.
 * Missing or wrong gets you stopped at the door — the second pitfall of wiring up Discord.
 */
const USER_AGENT = "DiscordBot (https://github.com/joyczl-agent, 0.0.0)";

/**
 * Which event types to listen to.
 *
 * Discord treats these as **switches**: without requesting them you don't receive the events —
 * it's not filter-after-receipt. `MESSAGE_CONTENT` is a **privileged** switch — the bot's
 * "Message Content Intent" must first be enabled in the developer portal, or the connection is
 * cut outright (4013). The step most likely to trip people up; the entry file repeats it too.
 */
const INTENTS =
  (1 << 0) | // GUILDS
  (1 << 9) | // GUILD_MESSAGES
  (1 << 12) | // DIRECT_MESSAGES
  (1 << 15); // MESSAGE_CONTENT

/** Gateway frame opcodes. Only the ones actually used. */
const OP = {
  DISPATCH: 0,
  HEARTBEAT: 1,
  IDENTIFY: 2,
  RECONNECT: 7,
  INVALID_SESSION: 9,
  HELLO: 10,
  HEARTBEAT_ACK: 11,
} as const;

// ---- Discord shapes ---------------------------------------------------------
// These are Discord's protocol, not Joy's, so they're absent from the generated artifacts and
// must be handwritten. Only the fields actually used are written down.

interface DiscordUser {
  id: string;
  /** The unique login name; changeable with a cooldown — the allowlist matches on it. */
  username?: string;
  /** Display name, changeable to anything, so it can't identify anyone. */
  global_name?: string | null;
  /** Every other bot carries this (including our own messages). */
  bot?: boolean;
}

interface DiscordMessage {
  channel_id: string;
  /** Absent in DMs — that's how "in a server" and "in a DM" are told apart. */
  guild_id?: string;
  content: string;
  author?: DiscordUser;
  mentions?: DiscordUser[];
}

interface GatewayFrame {
  op: number;
  /** Sequence number, only on DISPATCH frames. Heartbeats must carry it so the server can tell. */
  s?: number;
  /** Event name, only on DISPATCH frames. */
  t?: string;
  d?: unknown;
}

/**
 * The parts of the gateway WebSocket the adapter actually uses.
 *
 * Such a small seam exists for testing: heartbeats, reconnects, mention detection, splitting are
 * pure logic — proving them shouldn't require connecting to Discord (a bot, app, approval first).
 */
export interface DiscordSocket {
  send(data: string): void;
  close(): void;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
}

/** Make one connection. */
export type DiscordSocketFactory = (url: string) => DiscordSocket;

/**
 * Defaults to Node's built-in global `WebSocket` (a standard feature since 22; no `ws` needed).
 *
 * One hand-off required: the standard `WebSocket`'s `onmessage` receives a `MessageEvent`, while
 * the seam above only asks for `{ data }` — we only touch `data`, but the types don't line up.
 */
const defaultSocketFactory: DiscordSocketFactory = (url) =>
  new WebSocket(url) as unknown as DiscordSocket;

export interface DiscordAdapterOptions {
  token: string;
  /**
   * Who may use it. Numeric ids or `@login names`. **Unset means anyone** — handy when trying
   * it on your own machine, but a public bot without it lends your model quota to the whole
   * internet.
   */
  allow?: readonly string[];
  /** Swap fetch for tests; the global one by default. */
  fetch?: typeof globalThis.fetch;
  /** Swap the WebSocket for tests; Node's built-in by default. */
  socket?: DiscordSocketFactory;
  onLog?: (text: string) => void;
}

/**
 * The Discord adapter.
 *
 * Same shape as the Telegram one (connect, receive, ask Joy, send the reply back); only the
 * transport differs: Telegram is long polling (offset pushed by hand), Discord is WebSocket
 * (heartbeats, IDENTIFY, reconnects) — everything below is Discord's own rules, nothing to do
 * with Joy.
 *
 * **RESUME is not implemented**: a reconnect is a fresh IDENTIFY, so messages spoken during the
 * dropped seconds are lost. Supporting it means remembering `session_id` and
 * `resume_gateway_url` — the only persistent state on this chain, deliberately skipped for now.
 * The cost: restarting this gateway loses messages; restarting the Telegram one doesn't.
 */
export class DiscordAdapter {
  readonly #token: string;
  readonly #allow: ReadonlySet<string>;
  readonly #fetch: typeof globalThis.fetch;
  readonly #open: DiscordSocketFactory;
  readonly #respond: Responder;
  readonly #onLog: (text: string) => void;

  /** Who we are. Only known from READY — needed to judge "was I @-mentioned". */
  #selfId: string | null = null;
  #socket: DiscordSocket | undefined;
  #failures = 0;
  #stopped = false;

  constructor(options: DiscordAdapterOptions, respond: Responder) {
    this.#token = options.token;
    this.#fetch = options.fetch ?? globalThis.fetch;
    this.#open = options.socket ?? defaultSocketFactory;
    this.#respond = respond;
    this.#onLog = options.onLog ?? ((text) => console.error(text));
    this.#allow = new Set(
      (options.allow ?? []).map((entry) => entry.trim().toLowerCase()),
    );
  }

  /** Loops until `stop()`. Never returns in normal operation. */
  async run(): Promise<void> {
    while (!this.#stopped) {
      try {
        await this.#connect();
        // A connection reached clears the failure count: one drop shouldn't lengthen the next wait.
        this.#failures = 0;
        if (this.#stopped) return;
        // **Wait a while even on a clean disconnect**. Discord takes connections down on purpose
        // (by design, not a fault) and expects you not to reconnect immediately — with a bad
        // token it closes with 4004 every time, and an instant reconnect is a self-harming hot loop.
        await this.#pause("the connection dropped");
      } catch (error) {
        if (this.#stopped) return;
        await this.#pause("cannot connect", error);
      }
    }
  }

  stop(): void {
    this.#stopped = true;
    this.#socket?.close();
  }

  /**
   * Connect once, until it drops.
   *
   * resolve = the connection ended (clean close, server asked to reconnect), reject = connecting
   * failed. Both are carried on by `run()`'s loop.
   */
  #connect(): Promise<void> {
    return new Promise((resolve, reject) => {
      let socket: DiscordSocket;
      try {
        socket = this.#open(GATEWAY_URL);
      } catch (error) {
        reject(error instanceof Error ? error : new Error(String(error)));
        return;
      }
      this.#socket = socket;

      /** Sequence of the latest DISPATCH. Heartbeats carry it so the server can tell. */
      let seq: number | null = null;
      let heartbeat: ReturnType<typeof setInterval> | undefined;
      let finished = false;

      const finish = (error?: unknown): void => {
        if (finished) return;
        finished = true;
        if (heartbeat !== undefined) clearInterval(heartbeat);
        // Close once proactively. On a failed connect Node may report only `error`, never
        // `close` — that dangling handle keeps the process alive: `run()` returned, yet the
        // process won't exit. Closing an already-closed socket is a no-op, so both paths are safe.
        socket.close();
        if (this.#socket === socket) this.#socket = undefined;
        if (error === undefined) resolve();
        else reject(error);
      };

      socket.onmessage = (event) => {
        let frame: GatewayFrame;
        try {
          frame = JSON.parse(String(event.data)) as GatewayFrame;
        } catch {
          // Unrecognizable frames are just dropped — they must not take the whole chain down.
          return;
        }
        if (typeof frame.s === "number") seq = frame.s;

        switch (frame.op) {
          case OP.HELLO: {
            // Start the heartbeat, then IDENTIFY: the server counts your liveness by the interval
            // given in HELLO — being a period late already lands you in the "suspicious" bucket.
            heartbeat = setInterval(() => {
              socket.send(JSON.stringify({ op: OP.HEARTBEAT, d: seq }));
            }, heartbeatInterval(frame.d));
            socket.send(
              JSON.stringify({
                op: OP.IDENTIFY,
                d: {
                  token: this.#token,
                  intents: INTENTS,
                  properties: {
                    os: process.platform,
                    browser: "joy",
                    device: "joy",
                  },
                },
              }),
            );
            return;
          }
          case OP.DISPATCH:
            this.#dispatch(frame);
            return;
          case OP.RECONNECT:
          case OP.INVALID_SESSION:
            // Both mean "this connection is unusable". Just close it — the loop above reconnects.
            socket.close();
            return;
          default:
            // HEARTBEAT_ACK lands here: receiving it means the link is alive; nothing to do.
            return;
        }
      };

      socket.onclose = () => finish();
      socket.onerror = () => finish(new Error("the Discord connection dropped"));
    });
  }

  #dispatch(frame: GatewayFrame): void {
    if (frame.t === "READY") {
      const self = selfOf(frame.d);
      this.#selfId = self?.id ?? null;
      this.#onLog(`connected as ${self?.username ?? "?"}`);
      return;
    }
    if (frame.t !== "MESSAGE_CREATE") return;
    // Accepting a message means catching its failures. Uncaught, a failed reply (kicked from a
    // channel, rate-limited into giving up, network gone) becomes an unhandled rejection — Node
    // takes the process down by default, and this process serves everyone's sessions.
    void this.#handle(frame.d as DiscordMessage).catch((error: unknown) => {
      this.#onLog(
        `this message was not handled: ${error instanceof Error ? error.message : String(error)}`,
      );
    });
  }

  async #handle(message: DiscordMessage): Promise<void> {
    const author = message.author;
    // What other bots say doesn't count — including our own last reply, or we'd chat with ourselves.
    if (author === undefined || author.bot === true) return;

    const channelId = message.channel_id;

    // In server channels: only speak when called. A Discord bot sees every message in a channel,
    // and one that chimes in on everything gets kicked from the server the next day. DMs don't
    // have this problem (it's just the two of you there), no @ needed.
    if (message.guild_id !== undefined && !this.#mentioned(message)) return;

    if (!this.#allowed(author)) {
      // Ignore silently, and don't reply "no permission" — that tells a stranger the bot is
      // alive and invites more probing. A trace in the logs is enough (same rule as Telegram).
      this.#onLog(`ignored someone outside the allowlist: ${author.id} (channel ${channelId})`);
      return;
    }

    const text = this.#stripMention(message.content);
    if (text === "") {
      await this.#say(channelId, "I cannot read this one — text only for now; voice and images are not wired up yet.");
      return;
    }

    let reply: string;
    try {
      reply = await this.#respond(`discord:${channelId}`, text);
    } catch (error) {
      // A reply explaining why beats silence — the chat box offers the user no other clue.
      // Only errors from **asking Joy** get translated this way: failing to send is another
      // matter; mixing them in produces misdirection like "can't reach Joy".
      reply = explain(error);
    }
    await this.#say(channelId, reply);
  }

  /** Was I @-mentioned. `@everyone` never appears in `mentions`, so it doesn't count. */
  #mentioned(message: DiscordMessage): boolean {
    const self = this.#selfId;
    return self !== null && (message.mentions ?? []).some((user) => user.id === self);
  }

  /**
   * Strip mention markers like `<@123>`.
   *
   * It's just "someone called you", not what's being said — left in, the model wastes its turn
   * guessing the digit string. `<@!123>` is the old form (nickname mention); both accepted.
   */
  #stripMention(content: string): string {
    const self = this.#selfId;
    if (self === null) return content.trim();
    return content
      .replaceAll(`<@${self}>`, "")
      .replaceAll(`<@!${self}>`, "")
      .trim();
  }

  #allowed(author: DiscordUser): boolean {
    if (this.#allow.size === 0) return true;
    if (this.#allow.has(author.id)) return true;
    return (
      author.username !== undefined &&
      this.#allow.has(`@${author.username.toLowerCase()}`)
    );
  }

  async #say(channelId: string, text: string): Promise<void> {
    for (const chunk of splitMessage(text, MESSAGE_LIMIT)) {
      await this.#post(channelId, chunk);
    }
  }

  async #post(channelId: string, content: string): Promise<void> {
    for (let attempt = 0; ; attempt += 1) {
      const response = await this.#fetch(
        `${API_URL}/channels/${channelId}/messages`,
        {
          method: "POST",
          headers: {
            "content-type": "application/json",
            authorization: `Bot ${this.#token}`,
            "user-agent": USER_AGENT,
          },
          body: JSON.stringify({ content }),
        },
      );

      if (response.status !== 429) {
        if (!response.ok) {
          throw new Error(
            `Discord refused the message: ${response.status} ${await response.text()}`,
          );
        }
        return;
      }

      // 429: Discord writes "how long to wait" in the response. Easy to hit when a long reply
      // splits into 7-8 chunks; wait as told and send again — the dropped chunks are invisible.
      if (attempt >= 2) throw new Error("Discord keeps rate limiting; this one was not sent");
      await sleep(await retryAfterMs(response));
    }
  }

  /**
   * Rest a while after a drop, then reconnect.
   *
   * A dropped connection must not kill the bot: a laptop waking, a hotspot switching cells — all
   * "wait a moment and it's fine"; an exiting process needs a human to restart. Backoff caps at 30s.
   *
   * Sleep in small slices while waiting, so `stop()` wakes it immediately — otherwise Ctrl-C
   * would sit out the whole backoff before exiting.
   */
  async #pause(reason: string, error?: unknown): Promise<void> {
    this.#failures += 1;
    const waitMs = Math.min(30_000, 1000 * 2 ** Math.min(this.#failures, 5));
    const detail =
      error === undefined
        ? ""
        : `：${error instanceof Error ? error.message : String(error)}`;
    this.#onLog(
      `${reason} (failure ${this.#failures}); reconnecting in ${Math.round(waitMs / 1000)} s${detail}`,
    );

    const until = Date.now() + waitMs;
    while (!this.#stopped && Date.now() < until) {
      await sleep(Math.min(200, until - Date.now()));
    }
  }
}

/** Heartbeat interval from HELLO (ms). Unrecognizable: 41.25s — Discord's usual value. */
function heartbeatInterval(payload: unknown): number {
  const value = (payload as { heartbeat_interval?: unknown } | undefined)
    ?.heartbeat_interval;
  return typeof value === "number" && value > 0 ? value : 41_250;
}

function selfOf(payload: unknown): DiscordUser | undefined {
  return (payload as { user?: DiscordUser } | undefined)?.user;
}

/** `retry_after` in a 429 response is seconds. Unrecognizable: one second — better than a double hit. */
async function retryAfterMs(response: Response): Promise<number> {
  try {
    const body = (await response.json()) as { retry_after?: unknown };
    return typeof body.retry_after === "number" ? Math.max(0, body.retry_after) * 1000 : 1000;
  } catch {
    return 1000;
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
