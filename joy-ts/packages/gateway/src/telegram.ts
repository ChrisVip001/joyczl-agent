import { explain } from "./bridge.ts";
import type { Responder } from "./bridge.ts";
import { splitMessage } from "./text.ts";

// `Responder` (the adapter-to-Joy interface) and `splitMessage` (splits long replies by the cap)
// are shared by both chat platforms, so their real definitions live elsewhere. Re-exporting here
// keeps callers and tests pointing at this one module as before.
export type { Responder } from "./bridge.ts";
export { splitMessage } from "./text.ts";

/** Telegram's per-message cap. Past it the API errors out; it won't truncate for you. */
export const MESSAGE_LIMIT = 4096;

// ---- Telegram API shapes ----------------------------------------------------
// These are Telegram's protocol, not Joy's, so they're absent from the generated artifacts and
// must be handwritten. Only the fields actually used are written down.

interface TelegramChat {
  id: number;
  type: string;
}

interface TelegramUser {
  id: number;
  username?: string;
  first_name?: string;
}

interface TelegramMessage {
  message_id: number;
  chat: TelegramChat;
  from?: TelegramUser;
  text?: string;
}

interface TelegramUpdate {
  update_id: number;
  message?: TelegramMessage;
}

interface TelegramEnvelope<T> {
  ok: boolean;
  result: T;
  description?: string;
}

export interface TelegramAdapterOptions {
  token: string;
  /**
   * Who may use it. Numeric ids or `@usernames`. **Unset means anyone** — handy when trying it
   * on your own machine, but a public bot without it lends your model quota to the whole
   * internet.
   */
  allow?: readonly string[];
  /** How long one long poll hangs (seconds). */
  pollSeconds?: number;
  /** Swap fetch for tests; the global one by default. */
  fetch?: typeof globalThis.fetch;
  onLog?: (text: string) => void;
}

/**
 * The Telegram adapter.
 *
 * Long polling instead of a webhook: a webhook needs a public HTTPS address, a certificate, and
 * a register/unregister lifecycle to manage. Long polling runs wherever there's outbound
 * network — `TELEGRAM_BOT_TOKEN=… npm start:telegram` on a laptop and it's alive.
 */
export class TelegramAdapter {
  readonly #token: string;
  readonly #allow: ReadonlySet<string>;
  readonly #pollSeconds: number;
  readonly #fetch: typeof globalThis.fetch;
  readonly #respond: Responder;
  readonly #onLog: (text: string) => void;
  readonly #abort = new AbortController();

  #failures = 0;
  #stopped = false;

  constructor(options: TelegramAdapterOptions, respond: Responder) {
    this.#token = options.token;
    this.#pollSeconds = options.pollSeconds ?? 30;
    this.#fetch = options.fetch ?? globalThis.fetch;
    this.#respond = respond;
    this.#onLog = options.onLog ?? ((text) => console.error(text));
    this.#allow = new Set(
      (options.allow ?? []).map((entry) => entry.trim().toLowerCase()),
    );
  }

  /** Loops until `stop()`. Never returns in normal operation. */
  async run(): Promise<void> {
    let offset = 0;

    while (!this.#stopped) {
      try {
        const updates = await this.#call<TelegramUpdate[]>("getUpdates", {
          offset,
          timeout: this.#pollSeconds,
          allowed_updates: ["message"],
        });
        this.#failures = 0;

        for (const update of updates) {
          // Advance the offset before handling: a crash mid-handling must not re-deliver the
          // same message, or the user sees duplicate replies.
          offset = update.update_id + 1;
          const message = update.message;
          if (message !== undefined) {
            // Accepting a message means catching its failures. Uncaught, a failed reply (the
            // user blocked the bot, the bot was kicked from a group) becomes an unhandled
            // rejection — Node takes the process down by default, and this process serves everyone.
            void this.#handle(message).catch((error: unknown) => {
              this.#onLog(
                `这条消息没处理完：${error instanceof Error ? error.message : String(error)}`,
              );
            });
          }
        }
      } catch (error) {
        if (this.#stopped) return;
        await this.#backoff(error);
      }
    }
  }

  stop(): void {
    this.#stopped = true;
    // The long poll is most likely hanging on a getUpdates right now; abort it for a clean exit.
    this.#abort.abort();
  }

  /**
   * A network hiccup must not kill the bot.
   *
   * Backoff capped at 30 seconds: Telegram's occasional 502s, a laptop waking from sleep, a
   * phone hotspot switching cells — all "wait a moment and it's fine". A process that exits
   * outright can only be restarted by a human typing the command again.
   */
  async #backoff(error: unknown): Promise<void> {
    this.#failures += 1;
    const waitMs = Math.min(30_000, 1000 * 2 ** Math.min(this.#failures, 5));
    this.#onLog(
      `长轮询失败（第 ${this.#failures} 次），${Math.round(waitMs / 1000)} 秒后重试：${
        error instanceof Error ? error.message : String(error)
      }`,
    );
    await new Promise((resolve) => setTimeout(resolve, waitMs));
  }

  async #handle(message: TelegramMessage): Promise<void> {
    const chatId = message.chat.id;

    if (!this.#allowed(message.from)) {
      // Ignore silently, and don't reply "no permission" — that tells a stranger the bot is
      // alive and invites more probing. A trace in the logs is enough.
      this.#onLog(
        `忽略了不在白名单里的人：${message.from?.id ?? "?"}（chat ${chatId}）`,
      );
      return;
    }

    // Sessions split by **chat**, not by person: a group shares one conversation or nobody can
    // keep up, and while @-mentioning around the context is one anyway.
    const conversation = `telegram:${chatId}`;

    const text = message.text;
    if (typeof text !== "string" || text.trim() === "") {
      await this.#say(chatId, "这条我读不了 —— 目前只认文字，语音和图片还没接。");
      return;
    }

    let reply: string;
    try {
      reply = await this.#respond(conversation, text);
    } catch (error) {
      // A reply explaining why beats silence — the chat box offers the user no other clue.
      // Only errors from **asking Joy** get translated this way: failing to send is another
      // matter; mixing them in produces misdirection like "can't reach Joy".
      reply = explain(error);
    }
    await this.#say(chatId, reply);
  }

  #allowed(from: TelegramUser | undefined): boolean {
    if (this.#allow.size === 0) return true;
    if (from === undefined) return false;
    if (this.#allow.has(String(from.id))) return true;
    return from.username !== undefined && this.#allow.has(`@${from.username.toLowerCase()}`);
  }

  async #say(chatId: number, text: string): Promise<void> {
    for (const chunk of splitMessage(text, MESSAGE_LIMIT)) {
      await this.#call("sendMessage", { chat_id: chatId, text: chunk });
    }
  }

  async #call<T>(method: string, body: unknown): Promise<T> {
    const response = await this.#fetch(
      `https://api.telegram.org/bot${this.#token}/${method}`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
        signal: this.#abort.signal,
      },
    );

    const envelope = (await response.json()) as TelegramEnvelope<T>;
    if (!envelope.ok) {
      throw new Error(
        `Telegram ${method} 失败：${envelope.description ?? response.status}`,
      );
    }
    return envelope.result;
  }
}


