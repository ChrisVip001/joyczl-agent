import { createServer } from "node:http";
import type { IncomingMessage, Server, ServerResponse } from "node:http";

import { explain } from "./bridge.ts";
import type { Responder } from "./bridge.ts";
import { splitMessage } from "./text.ts";
import { decryptMessage, encryptMessage, signatureOf } from "./wechat-crypto.ts";

// 调用方和测试照旧只认这一个模块（跟 telegram / discord 一样）。
export type { Responder } from "./bridge.ts";
export { splitMessage } from "./text.ts";

/**
 * 客服消息里一段文本的字符上限。
 *
 * 官方文档没写这个数 —— 翻遍了「发送客服消息」那页也只有 `content` 是
 * 「文本内容」。微信实际是按 **2048 字节** 卡的，而一个汉字占 3 字节，
 * 所以压在 600 字符就是 1800 字节，留了余量。
 *
 * 保守一点是有理由的：被动回复只能回**一条**，所以超过一条的回答本来就
 * 得走客服消息，而客服消息有额度（48 小时 5 条）—— 少切一条省一份额度，
 * 但切短了被拒，整段就白发了。
 */
export const MESSAGE_LIMIT = 600;

/**
 * 被动回复的时限：微信给 5 秒。
 *
 * 超了它会重试（一共三次），三次都没回就把用户那句话丢掉。1 秒留给网络
 * 和微信那头，所以自己掐在 4 秒。
 *
 * **这条时限决定了整个适配器的形状**：Joy 跑一轮要多久？带工具调用的那几
 * 轮远超 5 秒。所以主流路径不是「被动回复」，而是「先回 `success` 把微信
 * 打发走，再拿客服消息把答复推回去」。被动回复只在答得快的时候才用得上。
 */
export const DEFAULT_REPLY_BUDGET_MS = 4000;

/** 请求体的上限。微信的消息就几 KB，给到 128K 是防着有人拿它打内存。 */
const MAX_BODY = 128 * 1024;

export interface WeChatAdapterOptions {
  /** 服务器配置里填的那个 Token。 */
  token: string;
  appId: string;
  /**
   * 安全 / 兼容模式才有；明文模式不用。
   *
   * 留空就是明文模式 —— 微信那边选了什么模式，这里就得跟着是什么，
   * 配错的表现是「验证 URL 失败」或者「收到一堆解不开的密文」。
   */
  encodingAesKey?: string;
  /**
   * 客服消息要它。**不给就退化成「只在 4 秒内答得完才回话」**。
   *
   * 另外它还有个前提：客服接口**只有认证过的公众号能调**，未认证的个人
   * 订阅号会一直报 48001。这是微信那边的门槛，不是这里能绕过去的。
   */
  appSecret?: string;
  port?: number;
  /** 回调路径。微信那边填的 URL 的 path 部分。 */
  path?: string;
  /**
   * 谁可以用。写用户的 OpenID。**不填就是谁都能用** —— 关注了公众号的人
   * 都能使唤它，花的是你的模型额度。
   */
  allow?: readonly string[];
  replyBudgetMs?: number;
  fetch?: typeof globalThis.fetch;
  onLog?: (text: string) => void;
}

/** 从微信那口固定形状的 XML 里取一个字段。 */
export function tag(xml: string, name: string): string {
  const cdata = new RegExp(
    `<${name}><!\\[CDATA\\[([\\s\\S]*?)\\]\\]></${name}>`,
  ).exec(xml);
  if (cdata !== null) return cdata[1] ?? "";
  const plain = new RegExp(`<${name}>([\\s\\S]*?)</${name}>`).exec(xml);
  return plain?.[1] ?? "";
}

/**
 * 塞进 CDATA 之前先把 `]]>` 拆开。
 *
 * 不处理的话，一句包含 `]]>` 的答复会把整段 XML 弄坏 —— 微信那边报的是
 * 「回复失败」，而你会对着一段看起来没问题的文本找半天。
 */
function cdata(text: string): string {
  return `<![CDATA[${text.replaceAll("]]>", "]]]]><![CDATA[>")}]]>`;
}

/** 等到点为止：`work` 先完成就给它，`ms` 到了就给 `undefined`。 */
async function within<T>(
  ms: number,
  work: Promise<T>,
): Promise<T | undefined> {
  let timer: NodeJS.Timeout | undefined;
  const deadline = new Promise<undefined>((resolve) => {
    timer = setTimeout(() => resolve(undefined), ms);
  });
  try {
    return await Promise.race([work, deadline]);
  } finally {
    // 不清理的话，这个定时器会白等着把进程多吊几秒。
    clearTimeout(timer);
  }
}

/**
 * 微信适配器。
 *
 * 跟 Telegram、Discord 最大的不同：**没有能连出去的路**。微信不给长轮询、
 * 也不给 WebSocket，只能当个 HTTP 服务器等着被推。所以这是三个平台里唯一
 * 需要「公网可达 + 一个地址」的 —— 本地跑要配内网穿透。
 *
 * 于是形状也不一样：那两家的适配器是「连上去，一条一条收」，这里是
 * 「起个服务器，收一个请求，**5 秒内**给个交代」。
 */
export class WeChatAdapter {
  readonly #token: string;
  readonly #appId: string;
  readonly #encodingAesKey: string | undefined;
  readonly #appSecret: string | undefined;
  readonly #port: number;
  readonly #path: string;
  readonly #allow: ReadonlySet<string>;
  readonly #budgetMs: number;
  readonly #fetch: typeof globalThis.fetch;
  readonly #respond: Responder;
  readonly #onLog: (text: string) => void;

  #server: Server | undefined;
  #accessToken: string | undefined;
  #accessTokenUntil = 0;
  #accessTokenPending: Promise<string> | undefined;

  /**
   * 刚见过的 `MsgId`。
   *
   * 微信在 5 秒没等到回复时会**把同一个请求再发一遍**（一共三次）。不去重
   * 的话，同一条消息会问 Joy 三轮、推三份回答给用户 —— 而且因为它们几乎
   * 同时回来，用户看到的是一堆重复。
   */
  readonly #seen = new Set<string>();

  constructor(options: WeChatAdapterOptions, respond: Responder) {
    this.#token = options.token;
    this.#appId = options.appId;
    this.#encodingAesKey = options.encodingAesKey;
    this.#appSecret = options.appSecret;
    this.#port = options.port ?? 8080;
    this.#path = options.path ?? "/wechat";
    this.#budgetMs = options.replyBudgetMs ?? DEFAULT_REPLY_BUDGET_MS;
    this.#fetch = options.fetch ?? globalThis.fetch;
    this.#respond = respond;
    this.#onLog = options.onLog ?? ((text) => console.error(text));
    this.#allow = new Set((options.allow ?? []).map((entry) => entry.trim()));
  }

  /** 起服务器，一直转到 `stop()`。 */
  async run(): Promise<void> {
    const server = createServer((request, response) => {
      // 一个请求的处理整段兜住。没兜住的话，一次没预料到的异常会变成
      // 未捕获的拒绝 —— Node 默认因此把进程带走，而它管着所有人的会话。
      void this.#serve(request, response).catch((error: unknown) => {
        this.#onLog(
          `这个请求没处理完：${error instanceof Error ? error.message : String(error)}`,
        );
        if (!response.headersSent) response.writeHead(500);
        response.end();
      });
    });
    this.#server = server;

    await new Promise<void>((resolve) => {
      server.on("close", () => resolve());
      server.listen(this.#port, () => {
        this.#onLog(
          `在 ${this.#port} 端口等微信推 ${this.#path} —— ` +
            `微信那边要的是公网可达的 80 或 443，本地跑得先配内网穿透。`,
        );
        if (this.#appSecret === undefined) {
          this.#onLog(
            "没配 AppSecret：客服消息推不了。于是只有「4 秒内答得完」的消息能回，" +
              "慢一点的那些微信就把用户那句掐了。",
          );
        }
      });
    });
  }

  stop(): void {
    this.#server?.close();
  }

  /**
   * 实际在听的端口。
   *
   * 给 `port: 0`（让系统随便挑一个）用 —— 测试要靠它问出挑中了哪个，
   * 不然就只能先猜一个端口号，撞上了就是查半天才找得到的随机失败。
   */
  get port(): number {
    const address = this.#server?.address();
    if (address === null || address === undefined) return this.#port;
    return typeof address === "object" ? address.port : this.#port;
  }

  /** 一个请求进来：先看是哪一种，再分给对应的处理。 */
  async #serve(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const url = new URL(request.url ?? "/", "http://localhost");
    if (url.pathname !== this.#path) {
      response.writeHead(404);
      response.end();
      return;
    }

    // GET 只有一件事：「服务器配置」那一步微信来验这个地址是不是你的。
    // 它不看你的业务，只看你回的那串 echostr 对不对。
    if (request.method === "GET") {
      const query = url.searchParams;
      const signature = query.get("signature") ?? "";
      const timestamp = query.get("timestamp") ?? "";
      const nonce = query.get("nonce") ?? "";
      if (signatureOf(this.#token, timestamp, nonce) !== signature) {
        this.#onLog("URL 验证没通过：签名对不上。多半是 Token 填得跟微信那边不一样。");
        response.writeHead(403);
        response.end();
        return;
      }
      response.writeHead(200, { "content-type": "text/plain" });
      response.end(query.get("echostr") ?? "");
      return;
    }

    if (request.method !== "POST") {
      response.writeHead(405);
      response.end();
      return;
    }

    const xml = await this.#plainXml(request, url.searchParams);
    if (xml === undefined) {
      // 验签没过或者解不开。这时候**不能**回 `success` —— 那等于告诉
      // 微信「我收到了」，而其实这条我们根本没处理。
      response.writeHead(403);
      response.end();
      return;
    }
    await this.#handle(xml, url.searchParams, response);
  }

  /**
   * 把入站正文规约成明文 XML。
   *
   * 三种模式在这一步之后**就没有区别了** —— 明文模式直接就是它；兼容模式
   * 明文密文都带，拿密文解出来跟明文等价；安全模式只有密文。所以后面所有
   * 逻辑只认明文 XML，不必到处分三种情况写。
   */
  async #plainXml(
    request: IncomingMessage,
    query: URLSearchParams,
  ): Promise<string | undefined> {
    const body = await readBody(request);
    if (body === undefined) return undefined;

    const timestamp = query.get("timestamp") ?? "";
    const nonce = query.get("nonce") ?? "";
    const key = this.#encodingAesKey;

    if (!body.includes("<Encrypt>")) {
      if (
        signatureOf(this.#token, timestamp, nonce) !==
        (query.get("signature") ?? "")
      ) {
        this.#onLog("收到一条明文消息，但签名对不上，丢掉。");
        return undefined;
      }
      return body;
    }

    // **这一步要用 `msg_signature`，不能用 `signature`。** 文档专门叮嘱过：
    // 安全模式下判「这条是不是真的从微信来的」，只认它。
    const encrypt = tag(body, "Encrypt");
    if (
      signatureOf(this.#token, timestamp, nonce, encrypt) !==
      (query.get("msg_signature") ?? "")
    ) {
      this.#onLog("收到一条加密消息，但 msg_signature 对不上，丢掉。");
      return undefined;
    }
    if (key === undefined) {
      this.#onLog(
        "微信那边选的是加密模式，但没给 WECHAT_AES_KEY，这些消息解不开。",
      );
      return undefined;
    }

    try {
      return decryptMessage(encrypt, {
        encodingAesKey: key,
        appId: this.#appId,
      });
    } catch (error) {
      this.#onLog(
        `解不开这条密文：${error instanceof Error ? error.message : String(error)}`,
      );
      return undefined;
    }
  }

  async #handle(
    xml: string,
    query: URLSearchParams,
    response: ServerResponse,
  ): Promise<void> {
    const openid = tag(xml, "FromUserName");
    const account = tag(xml, "ToUserName");
    const msgId = tag(xml, "MsgId");

    if (msgId !== "") {
      if (this.#seen.has(msgId)) {
        // 微信的重试。第一次已经收下了，这里只要让它别再发 —— 再答一遍
        // 就是第二条重复的话，而且两条几乎同时到，用户看到的是一堆重影。
        this.#onLog(`微信又把 ${msgId} 推了一遍（5 秒没等到答复），跳过。`);
        this.#end(response, "success");
        return;
      }
      this.#remember(msgId);
    }

    if (this.#allow.size > 0 && !this.#allow.has(openid)) {
      // 不搭理，也不回一句「你没权限」—— 那等于告诉陌生人这个号是活的。
      this.#onLog(`忽略了不在白名单里的人：${openid}`);
      this.#end(response, "success");
      return;
    }

    const text = tag(xml, "MsgType") === "text" ? tag(xml, "Content") : "";
    const pending: Promise<string> =
      text.trim() === ""
        ? Promise.resolve("这条我读不了 —— 目前只认文字，语音和图片还没接。")
        : this.#respond(`wechat:${openid}`, text).catch((error: unknown) =>
            explain(error),
          );

    // 4 秒内答得完就用被动回复，答不完先回 `success`。
    const settled = await within(
      this.#budgetMs,
      pending.then((reply) => splitMessage(reply, MESSAGE_LIMIT)),
    );

    // 被动回复只能回**一条**，所以只有切成一条时才走这儿。多段回答不走
    // 「第一段就地回、剩下的推客服消息」—— 那会让顺序变成两处拼出来的，
    // 而这种小聪明在出问题时极难查。
    if (settled !== undefined && settled.length === 1) {
      this.#end(
        response,
        this.#replyXml(settled[0]!, openid, account, query.get("nonce") ?? ""),
      );
      return;
    }

    // 先把微信打发走 —— 它那 5 秒的钟还在走，而 Joy 那边可能还要几十秒。
    this.#end(response, "success");

    const chunks = settled ?? splitMessage(await pending, MESSAGE_LIMIT);
    for (const chunk of chunks) {
      try {
        await this.#sendCustom(openid, chunk);
      } catch (error) {
        // 一条发不出去就停下：后面几条一样会失败，而三条一样的错比一条
        // 有用的错难读得多。这里最常见的因由是账号没认证。
        this.#onLog(
          `客服消息没发出去：${error instanceof Error ? error.message : String(error)}`,
        );
        return;
      }
    }
  }

  /**
   * 被动回复的正文。
   *
   * 加密模式下回包也得是密文，而且外面那层 `<xml>` 里装的是
   * `Encrypt / MsgSignature / TimeStamp / Nonce` —— 不是消息本身。
   * `Nonce` 回填微信 URL 上那个就行（文档明说的），`TimeStamp` 用当前时间。
   */
  #replyXml(
    reply: string,
    openid: string,
    account: string,
    nonce: string,
  ): string {
    const time = Math.floor(Date.now() / 1000);
    // `ToUserName` 和 `FromUserName` 是**对调**的：回给谁、谁回的。
    const inner =
      "<xml>" +
      `<ToUserName>${cdata(openid)}</ToUserName>` +
      `<FromUserName>${cdata(account)}</FromUserName>` +
      `<CreateTime>${time}</CreateTime>` +
      `<MsgType>${cdata("text")}</MsgType>` +
      `<Content>${cdata(reply)}</Content>` +
      "</xml>";

    const key = this.#encodingAesKey;
    if (key === undefined) return inner;

    const encrypt = encryptMessage(inner, {
      encodingAesKey: key,
      appId: this.#appId,
    });
    return (
      "<xml>" +
      `<Encrypt>${cdata(encrypt)}</Encrypt>` +
      `<MsgSignature>${cdata(signatureOf(this.#token, String(time), nonce, encrypt))}</MsgSignature>` +
      `<TimeStamp>${time}</TimeStamp>` +
      `<Nonce>${cdata(nonce)}</Nonce>` +
      "</xml>"
    );
  }

  #end(response: ServerResponse, body: string): void {
    response.writeHead(200, {
      "content-type": body.startsWith("<xml")
        ? "text/xml; charset=utf-8"
        : "text/plain; charset=utf-8",
    });
    response.end(body);
  }

  #remember(msgId: string): void {
    this.#seen.add(msgId);
    // 只留最近的一批：这是个「刚见过没有」的判定，不是历史记录。微信的重试
    // 都在几秒内，200 条足够，而且 Set 记着插入顺序，淘汰的就是最老的。
    if (this.#seen.size > 200) {
      const oldest = this.#seen.values().next().value;
      if (oldest !== undefined) this.#seen.delete(oldest);
    }
  }

  /** 拿 `access_token`，带缓存。 */
  async #getAccessToken(): Promise<string> {
    const secret = this.#appSecret;
    if (secret === undefined) {
      throw new Error(
        "没配 AppSecret —— 客服消息发不出去。（另外它只有认证过的公众号能调。）",
      );
    }
    if (this.#accessToken !== undefined && Date.now() < this.#accessTokenUntil) {
      return this.#accessToken;
    }

    // 并发时只发一次请求：微信那边**每取一把新的，旧的立刻作废**，
    // 所以同时来的两句「去取」会让先取到的那把当场失效。
    this.#accessTokenPending ??= this.#fetchToken(secret).finally(() => {
      this.#accessTokenPending = undefined;
    });
    return this.#accessTokenPending;
  }

  async #fetchToken(secret: string): Promise<string> {
    const url =
      "https://api.weixin.qq.com/cgi-bin/token" +
      `?grant_type=client_credential&appid=${encodeURIComponent(this.#appId)}` +
      `&secret=${encodeURIComponent(secret)}`;
    const body = (await (await this.#fetch(url)).json()) as {
      access_token?: string;
      expires_in?: number;
      errcode?: number;
      errmsg?: string;
    };
    if (typeof body.access_token !== "string") {
      throw new Error(`取 access_token 没成：${body.errcode} ${body.errmsg}`);
    }

    this.#accessToken = body.access_token;
    // 提前五分钟作废：卡在到期的边儿上取，请求发出去的时候可能刚好过期。
    this.#accessTokenUntil =
      Date.now() + ((body.expires_in ?? 7200) - 300) * 1000;
    return body.access_token;
  }

  async #sendCustom(openid: string, content: string): Promise<void> {
    // 一次就够，不会循环：`attempt` 到 1 之后无论什么错都抛出去。
    for (let attempt = 0; ; attempt += 1) {
      const token = await this.#getAccessToken();
      const response = await this.#fetch(
        "https://api.weixin.qq.com/cgi-bin/message/custom/send" +
          `?access_token=${encodeURIComponent(token)}`,
        {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({
            touser: openid,
            msgtype: "text",
            text: { content },
          }),
        },
      );
      const body = (await response.json()) as {
        errcode: number;
        errmsg: string;
      };
      if (body.errcode === 0) return;

      // 40001 / 42001 都是「这把 token 不算数了」。缓存里那把可能是别处
      // 取过的，也可能刚好卡在过期的边界 —— 清掉再来一次，别让用户白等。
      if (
        attempt === 0 &&
        (body.errcode === 40001 || body.errcode === 42001)
      ) {
        this.#accessToken = undefined;
        this.#accessTokenUntil = 0;
        continue;
      }
      throw new Error(`客服消息被拒：${body.errcode} ${body.errmsg}`);
    }
  }
}

/** 把请求体读全。超长就直接放弃 —— 微信的消息就几 KB。 */
async function readBody(request: IncomingMessage): Promise<string | undefined> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of request) {
    const buffer = chunk as Buffer;
    size += buffer.length;
    if (size > MAX_BODY) return undefined;
    chunks.push(buffer);
  }
  return Buffer.concat(chunks).toString("utf8");
}
