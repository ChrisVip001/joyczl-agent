import { explain } from "./bridge.ts";
import type { Responder } from "./bridge.ts";
import { splitMessage } from "./text.ts";

/** Discord 单条消息的上限。超了 API 直接 400，不会替你截。 */
export const MESSAGE_LIMIT = 2000;

/**
 * 网关地址。`encoding=json` 是唯一需要支持的编码 —— 另一种（etf）是给
 * 每天几十万条事件的库用的。
 *
 * 这里用 WebSocket 而不是 webhook：webhook 要一个公网可达的 HTTPS 地址、
 * 一张证书、还得处理 Discord 的签名校验和「先回 200 再慢慢处理」那套。
 * 连出去就能跑，跟 Telegram 那边选长轮询是同一个理由。
 */
const GATEWAY_URL = "wss://gateway.discord.gg/?v=10&encoding=json";
const API_URL = "https://discord.com/api/v10";

/**
 * Discord 要求每个请求都报上名号，格式是 `DiscordBot (url, version)`。
 * 缺了或者写错会被挡在门口 —— 这是接 Discord 的第二个坑。
 */
const USER_AGENT = "DiscordBot (https://github.com/joyczl-agent, 0.0.0)";

/**
 * 要听哪几类事件。
 *
 * Discord 把它们当**开关**：不申请就收不到，不是收完再过滤。
 * `MESSAGE_CONTENT` 还是**特权**开关 —— 必须先去开发者门户把 bot 的
 * 「Message Content Intent」打开，否则这条连接会被直接掐掉（4013）。
 * 这是接 Discord 最容易卡住的一步，入口文件里也再说一遍。
 */
const INTENTS =
  (1 << 0) | // GUILDS
  (1 << 9) | // GUILD_MESSAGES
  (1 << 12) | // DIRECT_MESSAGES
  (1 << 15); // MESSAGE_CONTENT

/** 网关帧的操作码。只列用得到的几个。 */
const OP = {
  DISPATCH: 0,
  HEARTBEAT: 1,
  IDENTIFY: 2,
  RECONNECT: 7,
  INVALID_SESSION: 9,
  HELLO: 10,
  HEARTBEAT_ACK: 11,
} as const;

// ---- Discord 的形状 ---------------------------------------------------------
// 这些是 Discord 的协议，不是 Joy 的，所以生成物里没有它们，只能手写。
// 只写了用得到的字段。

interface DiscordUser {
  id: string;
  /** 唯一的登录名，能改但有冷却 —— 白名单认的是它。 */
  username?: string;
  /** 展示名，想改成什么都行，所以不能拿它认人。 */
  global_name?: string | null;
  /** 别的 bot（包括自己发的消息）都带这个。 */
  bot?: boolean;
}

interface DiscordMessage {
  channel_id: string;
  /** 私聊里没有这一项 —— 「在服务器里」和「在私聊里」就是这么分的。 */
  guild_id?: string;
  content: string;
  author?: DiscordUser;
  mentions?: DiscordUser[];
}

interface GatewayFrame {
  op: number;
  /** 序号，只有 DISPATCH 帧带。心跳要带上它，服务端靠它判断你漏没漏。 */
  s?: number;
  /** 事件名，只有 DISPATCH 帧有。 */
  t?: string;
  d?: unknown;
}

/**
 * 网关那条 WebSocket 上，适配器真正用得到的部分。
 *
 * 收成这么小的一个口子是为了能测：心跳、重连、@ 的判定、切长消息全是纯逻辑，
 * 不该为了测它们真的去连一次 Discord —— 那得先有一个 bot、一个应用、一次审批。
 */
export interface DiscordSocket {
  send(data: string): void;
  close(): void;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
}

/** 造一条连接。 */
export type DiscordSocketFactory = (url: string) => DiscordSocket;

/**
 * 默认用 Node 自带的全局 `WebSocket`（22 起是标准件，不用装 `ws`）。
 *
 * 得转一道手：标准 `WebSocket` 的 `onmessage` 收 `MessageEvent`，而上面那个
 * 口子只要求 `{ data }` —— 我们只碰 `data`，但类型上对不齐，只能绕过去。
 */
const defaultSocketFactory: DiscordSocketFactory = (url) =>
  new WebSocket(url) as unknown as DiscordSocket;

export interface DiscordAdapterOptions {
  token: string;
  /**
   * 谁可以用。写数字 id 或 `@登录名`。**不填就是谁都能用** ——
   * 自己在机器上试的时候方便，但一个放出去的 bot 不设它，等于把自己的
   * 模型额度借给整个互联网。
   */
  allow?: readonly string[];
  /** 换掉 fetch 来测，默认用全局的。 */
  fetch?: typeof globalThis.fetch;
  /** 换掉 WebSocket 来测，默认用 Node 自带的。 */
  socket?: DiscordSocketFactory;
  onLog?: (text: string) => void;
}

/**
 * Discord 适配器。
 *
 * 跟 Telegram 那个是同一个形状（连上、收消息、问 Joy、把答复发回去），
 * 差别只在传输：Telegram 是长轮询（要自己推 offset），Discord 是 WebSocket
 * （要心跳、要 IDENTIFY、断了要重连）—— 下面这些都是 Discord 自己的规矩，
 * 跟 Joy 一点关系都没有。
 *
 * **不实现 RESUME**：断线重连是重新 IDENTIFY，所以断的那几秒里说的话会丢。
 * 要补上得记住 `session_id` 和 `resume_gateway_url` —— 那会是这条链上唯一
 * 需要持久状态的地方，先不做。代价说清楚：重启网关会漏消息，重启 Telegram
 * 网关不会（那边 offset 一推，没拿到的还会再来）。
 */
export class DiscordAdapter {
  readonly #token: string;
  readonly #allow: ReadonlySet<string>;
  readonly #fetch: typeof globalThis.fetch;
  readonly #open: DiscordSocketFactory;
  readonly #respond: Responder;
  readonly #onLog: (text: string) => void;

  /** 自己是谁。READY 里才知道 —— 判断「有没有 @ 我」得用它。 */
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

  /** 一直转到 `stop()`。正常跑不会返回。 */
  async run(): Promise<void> {
    while (!this.#stopped) {
      try {
        await this.#connect();
        // 连上过就算把之前的失败记录清零：一次断线不该让下一次重连也等很久。
        this.#failures = 0;
        if (this.#stopped) return;
        // **正常断开也要等一会儿**。Discord 会主动让连接下来（这是它的设计，
        // 不是故障），而且指望你别立刻连回来 —— token 不对时它每次都立刻
        // 以 4004 关掉，马上重连就是个打自己的热循环。
        await this.#pause("连接断了");
      } catch (error) {
        if (this.#stopped) return;
        await this.#pause("连不上", error);
      }
    }
  }

  stop(): void {
    this.#stopped = true;
    this.#socket?.close();
  }

  /**
   * 连一次，直到它断掉。
   *
   * resolve = 连接结束了（正常关闭、服务端让重连），reject = 连的过程出错。
   * 两种都由 `run()` 那圈循环接着往下办。
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

      /** 最近一次 DISPATCH 的序号。心跳带上它，服务端才知道你漏没漏。 */
      let seq: number | null = null;
      let heartbeat: ReturnType<typeof setInterval> | undefined;
      let finished = false;

      const finish = (error?: unknown): void => {
        if (finished) return;
        finished = true;
        if (heartbeat !== undefined) clearInterval(heartbeat);
        // 主动关一次。连接失败时 Node 底下只报 `error`、不一定再给 `close`
        // —— 那个句柄会把进程一直吊着：`run()` 明明返回了，进程却不退。
        // 已经关掉的时候再关一次是空操作，所以这条对两条路径都安全。
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
          // 认不出的帧丢掉就好 —— 它不该有能力把整条链带下去。
          return;
        }
        if (typeof frame.s === "number") seq = frame.s;

        switch (frame.op) {
          case OP.HELLO: {
            // 先起心跳、再 IDENTIFY：服务端按 HELLO 里给的间隔数你活着没有，
            // 晚开口一个周期就已经在「可疑」那一档了。
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
            // 两种都是「这条连接不能用了」。关掉就是 —— 上面的循环会连回来。
            socket.close();
            return;
          default:
            // HEARTBEAT_ACK 落在这儿：能收到它就说明链路是活的，不用做什么。
            return;
        }
      };

      socket.onclose = () => finish();
      socket.onerror = () => finish(new Error("Discord 的连接断了"));
    });
  }

  #dispatch(frame: GatewayFrame): void {
    if (frame.t === "READY") {
      const self = selfOf(frame.d);
      this.#selfId = self?.id ?? null;
      this.#onLog(`以 ${self?.username ?? "?"} 的身份连上了`);
      return;
    }
    if (frame.t !== "MESSAGE_CREATE") return;
    // 收下这一条就得兜住它。没兜住的话，回话失败（被踢出频道、限流到放弃、
    // 网断）会变成一次未捕获的拒绝 —— Node 默认就因此把进程带走，而这个
    // 进程管的是所有人的会话，不该因为一个人那儿发不出去就整条倒下。
    void this.#handle(frame.d as DiscordMessage).catch((error: unknown) => {
      this.#onLog(
        `这条消息没处理完：${error instanceof Error ? error.message : String(error)}`,
      );
    });
  }

  async #handle(message: DiscordMessage): Promise<void> {
    const author = message.author;
    // 别的 bot 说的话不算 —— 包括自己刚发出去的那条，否则会跟自己聊起来。
    if (author === undefined || author.bot === true) return;

    const channelId = message.channel_id;

    // 服务器频道里：只有叫了你才接话。Discord 的 bot 能看见频道里每一条消息，
    // 而一个对每句话都插嘴的 bot 第二天就会被踢出服务器。私聊里没这个问题
    // （那儿本来就只有你们俩），不必 @。
    if (message.guild_id !== undefined && !this.#mentioned(message)) return;

    if (!this.#allowed(author)) {
      // 不搭理，也不回一句「你没权限」—— 那等于告诉陌生人这个 bot 是活的，
      // 反而招来更多试探。日志里留个痕就够了。（跟 Telegram 那边同一条规矩。）
      this.#onLog(`忽略了不在白名单里的人：${author.id}（频道 ${channelId}）`);
      return;
    }

    const text = this.#stripMention(message.content);
    if (text === "") {
      await this.#say(channelId, "这条我读不了 —— 目前只认文字，语音和图片还没接。");
      return;
    }

    let reply: string;
    try {
      reply = await this.#respond(`discord:${channelId}`, text);
    } catch (error) {
      // 回一句说清为什么，比默不作声强 —— 用户在聊天框里没有任何别的线索。
      // 只有**问 Joy** 这一段的错才这么翻：发不出去是另一回事，混进来会
      // 变成「连不上 Joy」这种指错方向的话。
      reply = explain(error);
    }
    await this.#say(channelId, reply);
  }

  /** 有没有 @ 到我。`@everyone` 不会出现在 `mentions` 里，所以不算。 */
  #mentioned(message: DiscordMessage): boolean {
    const self = this.#selfId;
    return self !== null && (message.mentions ?? []).some((user) => user.id === self);
  }

  /**
   * 把 `<@123>` 这种提及标记摘掉。
   *
   * 它只是「叫了你一声」，不是要说的话 —— 留着它，模型每轮都要先猜
   * 这串数字是什么。`<@!123>` 是旧写法（按昵称提及），两种都认。
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
            `Discord 发消息失败：${response.status} ${await response.text()}`,
          );
        }
        return;
      }

      // 429：Discord 把「等多久」写在应答里。一段长回答切成七八条时很容易撞上，
      // 撞上就等它说的那么久再发一次 —— 丢掉的那几条用户是看不见的。
      if (attempt >= 2) throw new Error("Discord 一直在限流，这条没能发出去");
      await sleep(await retryAfterMs(response));
    }
  }

  /**
   * 断了之后歇一会儿再连。
   *
   * 连接断了不该让 bot 死掉：笔记本合盖再打开、手机热点换基站 —— 这些都是
   * 「等会儿就好」，而一个直接退出的进程只能靠人重新敲命令。退避封顶 30 秒。
   *
   * 等的时候按小段睡，好让 `stop()` 一进来就醒 —— 否则 Ctrl-C 之后还得
   * 干等退避到点才退得出去。
   */
  async #pause(reason: string, error?: unknown): Promise<void> {
    this.#failures += 1;
    const waitMs = Math.min(30_000, 1000 * 2 ** Math.min(this.#failures, 5));
    const detail =
      error === undefined
        ? ""
        : `：${error instanceof Error ? error.message : String(error)}`;
    this.#onLog(
      `${reason}（第 ${this.#failures} 次），${Math.round(waitMs / 1000)} 秒后重连${detail}`,
    );

    const until = Date.now() + waitMs;
    while (!this.#stopped && Date.now() < until) {
      await sleep(Math.min(200, until - Date.now()));
    }
  }
}

/** HELLO 里的心跳间隔（毫秒）。认不出来就按 41.25 秒 —— Discord 常给的值。 */
function heartbeatInterval(payload: unknown): number {
  const value = (payload as { heartbeat_interval?: unknown } | undefined)
    ?.heartbeat_interval;
  return typeof value === "number" && value > 0 ? value : 41_250;
}

function selfOf(payload: unknown): DiscordUser | undefined {
  return (payload as { user?: DiscordUser } | undefined)?.user;
}

/** 429 应答里的 `retry_after` 是秒。认不出来就按一秒来 —— 总比立刻重发又撞一次强。 */
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
