import { explain } from "./bridge.ts";
import type { Responder } from "./bridge.ts";
import { splitMessage } from "./text.ts";

// `Responder`（适配器与 Joy 的接口）和 `splitMessage`（按上限切长回答）
// 都是两个聊天平台共用的，所以真正的定义不在这儿。在这儿重新导出，
// 是为了调用方和测试照旧只认这一个模块。
export type { Responder } from "./bridge.ts";
export { splitMessage } from "./text.ts";

/** Telegram 单条消息的上限。超了 API 直接报错，不会替你截。 */
export const MESSAGE_LIMIT = 4096;

// ---- Telegram API 的形状 ----------------------------------------------------
// 这些是 Telegram 的协议，不是 Joy 的，所以生成物里没有它们，只能手写。
// 只写了用得到的字段。

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
   * 谁可以用。写数字 id 或 `@用户名`。**不填就是谁都能用** ——
   * 自己在机器上试的时候方便，但一个放出去的 bot 不设它，等于把自己的
   * 模型额度借给整个互联网。
   */
  allow?: readonly string[];
  /** 长轮询一次挂多久（秒）。 */
  pollSeconds?: number;
  /** 换掉 fetch 来测，默认用全局的。 */
  fetch?: typeof globalThis.fetch;
  onLog?: (text: string) => void;
}

/**
 * Telegram 适配器。
 *
 * 长轮询而不是 webhook：webhook 要一个公网可达的 HTTPS 地址、一张证书、
 * 还有一条「注册/注销 webhook」的生命周期要管。长轮询只要能出网就能跑，
 * 一台笔记本上 `TELEGRAM_BOT_TOKEN=… npm start:telegram` 就活了。
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

  /** 一直转到 `stop()`。正常跑不会返回。 */
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
          // 先推进 offset 再处理：处理时崩了也不该把同一条消息再收一遍，
          // 那会让用户看到重复的回答。
          offset = update.update_id + 1;
          const message = update.message;
          if (message !== undefined) {
            // 收下这一条就得兜住它。没兜住的话，回话失败（用户把 bot 封了、
            // 被踢出群）会变成一次未捕获的拒绝 —— Node 默认就因此把进程带走，
            // 而这个进程管着所有人的会话。
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
    // 长轮询这会儿多半正挂在一次 getUpdates 上，得把它掀了才退得干净。
    this.#abort.abort();
  }

  /**
   * 出网抖了一下不该让 bot 死掉。
   *
   * 退避到 30 秒封顶：Telegram 那边偶发 502、笔记本合盖再打开、
   * 手机热点换基站 —— 这些都是「等会儿就好」，而一个直接退出的进程
   * 只能靠人重新敲命令。
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
      // 不搭理，也不回一句「你没权限」—— 那等于告诉陌生人这个 bot 是活的，
      // 反而招来更多试探。日志里留个痕就够了。
      this.#onLog(
        `忽略了不在白名单里的人：${message.from?.id ?? "?"}（chat ${chatId}）`,
      );
      return;
    }

    // 会话按**聊天**分，不按人分：群里大家共用一段对话才聊得下去，
    // 而且 @ 来 @ 去的时候上下文本来就是一个。
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
      // 回一句说清为什么，比默不作声强 —— 用户在聊天框里没有任何别的线索。
      // 只有**问 Joy** 这一段的错才这么翻：发不出去是另一回事，混进来会
      // 变成「连不上 Joy」这种指错方向的话。
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


