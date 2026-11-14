import { createServer } from "node:http";
import type { IncomingMessage, Server, ServerResponse } from "node:http";

import { explain } from "./bridge.ts";
import type { Responder } from "./bridge.ts";
import { splitMessage } from "./text.ts";
import { decryptMessage, encryptMessage, signatureOf } from "./wechat-crypto.ts";

// Callers and tests keep pointing at this one module (same as telegram / discord).
export type { Responder } from "./bridge.ts";
export { splitMessage } from "./text.ts";

/**
 * Character cap for one text segment of a customer-service message.
 *
 * The official docs never state this number — the "send customer-service message" page only
 * says `content` is "text content". WeChat actually enforces **2048 bytes**, and a CJK char is
 * 3 bytes, so 600 chars = 1800 bytes, with headroom.
 *
 * The conservatism has a reason: passive replies can return **one** message only, so any longer
 * reply already goes via customer-service messages, which have a quota (5 per 48 hours) — fewer
 * chunks save quota, but a rejected chunk wastes the whole reply.
 */
export const MESSAGE_LIMIT = 600;

/**
 * The passive-reply deadline: WeChat gives 5 seconds.
 *
 * Past it WeChat retries (three times total), then drops the user's message. 1 second goes to
 * the network and WeChat's side, so we cut ourselves off at 4.
 *
 * **This deadline shapes the whole adapter**: how long does a Joy turn take? Turns with tool
 * calls far exceed 5 seconds. So the main path isn't "passive reply" but "reply `success` first,
 * then push the answer as customer-service messages". Passive replies only fit fast answers.
 */
export const DEFAULT_REPLY_BUDGET_MS = 4000;

/** Request body cap. WeChat messages are a few KB; 128K guards against memory-attack abuse. */
const MAX_BODY = 128 * 1024;

export interface WeChatAdapterOptions {
  /** The Token filled in under server configuration. */
  token: string;
  appId: string;
  /**
   * Only for safe / compatible mode; plain mode doesn't use it.
   *
   * Empty means plain mode — whatever mode WeChat's side chose, this must match; a mismatch
   * shows up as "URL verification failed" or "a pile of undecryptable ciphertext".
   */
  encodingAesKey?: string;
  /**
   * Customer-service messages need it. **Without it, degrade to "only replies that finish in 4s"**.
   *
   * It also has a prerequisite: the customer-service API is **for verified accounts only**;
   * unverified personal subscription accounts keep getting 48001 — WeChat's gate, not ours.
   */
  appSecret?: string;
  port?: number;
  /** Callback path. The path part of the URL filled in on WeChat's side. */
  path?: string;
  /**
   * Who may use it. Users' OpenIDs. **Unset means anyone** — every follower of the account can
   * command it, spending your model quota.
   */
  allow?: readonly string[];
  replyBudgetMs?: number;
  fetch?: typeof globalThis.fetch;
  onLog?: (text: string) => void;
}

/** Take one field out of WeChat's fixed-shape XML. */
export function tag(xml: string, name: string): string {
  const cdata = new RegExp(
    `<${name}><!\\[CDATA\\[([\\s\\S]*?)\\]\\]></${name}>`,
  ).exec(xml);
  if (cdata !== null) return cdata[1] ?? "";
  const plain = new RegExp(`<${name}>([\\s\\S]*?)</${name}>`).exec(xml);
  return plain?.[1] ?? "";
}

/**
 * Break up `]]>` before it goes into CDATA.
 *
 * Unhandled, a reply containing `]]>` corrupts the whole XML — WeChat reports "reply failed",
 * and you'd stare at a perfectly healthy-looking text for hours.
 */
function cdata(text: string): string {
  return `<![CDATA[${text.replaceAll("]]>", "]]]]><![CDATA[>")}]]>`;
}

/** Wait until the deadline: `work` finishing first wins; at `ms`, `undefined` wins. */
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
    // UnCleaned, this timer would idle around, keeping the process alive a few seconds extra.
    clearTimeout(timer);
  }
}

/**
 * The WeChat adapter.
 *
 * The biggest difference from Telegram and Discord: **no outbound connection**. WeChat offers
 * neither long polling nor WebSocket; you can only be an HTTP server waiting to be pushed — the
 * only one of the three needing "public reachability + one address"; local runs need a tunnel.
 *
 * The shape differs accordingly: those two adapters "connect and receive one by one"; here it's
 * "start a server, take one request, account for it **within 5 seconds**".
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
   * Recently seen `MsgId`s.
   *
   * When WeChat gets no reply within 5 seconds it **re-sends the same request** (three times
   * total). Without dedup, one message asks Joy three times and pushes three replies — arriving
   * nearly together, they read to the user as a pile of duplicates.
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

  /** Start the server, looping until `stop()`. */
  async run(): Promise<void> {
    const server = createServer((request, response) => {
      // Wrap the whole request handling. Unwrapped, one surprise exception becomes an unhandled
      // rejection — Node takes the process down by default, and it serves everyone's sessions.
      void this.#serve(request, response).catch((error: unknown) => {
        this.#onLog(
          `this request was not handled: ${error instanceof Error ? error.message : String(error)}`,
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
          `waiting for WeChat to push ${this.#path} on port ${this.#port} — ` +
            `WeChat wants a publicly reachable 80 or 443, so a local run needs a tunnel first.`,
        );
        if (this.#appSecret === undefined) {
          this.#onLog(
            "no AppSecret: customer-service messages cannot be pushed. So only messages that can be " +
            "answered within 4 seconds get a reply, " +
              "and WeChat cuts off the user's line on the slower ones.",
          );
        }
      });
    });
  }

  stop(): void {
    this.#server?.close();
  }

  /**
   * The port actually being listened on.
   *
   * For `port: 0` (let the system pick) — tests need it to learn which port was chosen, else
   * they'd guess one, and a collision becomes a random failure that takes ages to trace.
   */
  get port(): number {
    const address = this.#server?.address();
    if (address === null || address === undefined) return this.#port;
    return typeof address === "object" ? address.port : this.#port;
  }

  /** One request in: identify the kind first, then dispatch to the matching handler. */
  async #serve(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const url = new URL(request.url ?? "/", "http://localhost");
    if (url.pathname !== this.#path) {
      response.writeHead(404);
      response.end();
      return;
    }

    // GET has one job: during "server configuration" WeChat verifies this address is yours.
    // It ignores your business logic; only your echo of `echostr` matters.
    if (request.method === "GET") {
      const query = url.searchParams;
      const signature = query.get("signature") ?? "";
      const timestamp = query.get("timestamp") ?? "";
      const nonce = query.get("nonce") ?? "";
      if (signatureOf(this.#token, timestamp, nonce) !== signature) {
        this.#onLog("URL verification failed: the signature does not match. The Token is probably "
          + "different from the one on the WeChat side.");
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
      // Signature failed or decryption failed. **Do not** reply `success` here — that tells
      // WeChat "received", when in fact we never processed this one.
      response.writeHead(403);
      response.end();
      return;
    }
    await this.#handle(xml, url.searchParams, response);
  }

  /**
   * Normalize the inbound body into plaintext XML.
   *
   * After this step the three modes **have no difference** — plain mode is already it;
   * compatible mode carries both, and decrypting the ciphertext equals the plaintext; safe mode
   * has ciphertext only. All later logic deals with plaintext XML alone, no three-way branches.
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
        this.#onLog("a plaintext message arrived with a signature that does not match; dropped.");
        return undefined;
      }
      return body;
    }

    // **This step must use `msg_signature`, not `signature`.** The docs say so explicitly:
    // in safe mode, deciding "did this really come from WeChat" accepts only that one.
    const encrypt = tag(body, "Encrypt");
    if (
      signatureOf(this.#token, timestamp, nonce, encrypt) !==
      (query.get("msg_signature") ?? "")
    ) {
      this.#onLog("an encrypted message arrived with a msg_signature that does not match; dropped.");
      return undefined;
    }
    if (key === undefined) {
      this.#onLog(
        "the WeChat side is in encrypted mode but WECHAT_AES_KEY is missing, so these cannot be opened.",
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
        `cannot open this ciphertext: ${error instanceof Error ? error.message : String(error)}`,
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
        // WeChat's retry. The first attempt was accepted; just make it stop resending — another
        // reply is a second duplicate, and arriving almost together they read as ghosts.
        this.#onLog(`WeChat pushed ${msgId} again (no answer within 5 s); skipped.`);
        this.#end(response, "success");
        return;
      }
      this.#remember(msgId);
    }

    if (this.#allow.size > 0 && !this.#allow.has(openid)) {
      // Ignore silently, and don't reply "no permission" — that tells a stranger the account is alive.
      this.#onLog(`ignored someone outside the allowlist: ${openid}`);
      this.#end(response, "success");
      return;
    }

    const text = tag(xml, "MsgType") === "text" ? tag(xml, "Content") : "";
    const pending: Promise<string> =
      text.trim() === ""
        ? Promise.resolve("I cannot read this one — text only for now; voice and images are not wired up yet.")
        : this.#respond(`wechat:${openid}`, text).catch((error: unknown) =>
            explain(error),
          );

    // If it finishes within 4s, use the passive reply; otherwise send `success` first.
    const settled = await within(
      this.#budgetMs,
      pending.then((reply) => splitMessage(reply, MESSAGE_LIMIT)),
    );

    // A passive reply can be **one** message only, so this path runs only for a single chunk.
    // Multi-chunk replies don't do "first chunk inline, rest via customer service" — that makes
    // the ordering a joint product of two places, and such cleverness is murder to debug.
    if (settled !== undefined && settled.length === 1) {
      this.#end(
        response,
        this.#replyXml(settled[0]!, openid, account, query.get("nonce") ?? ""),
      );
      return;
    }

    // Send WeChat away first — its 5-second clock is still running, and Joy may need tens of seconds.
    this.#end(response, "success");

    const chunks = settled ?? splitMessage(await pending, MESSAGE_LIMIT);
    for (const chunk of chunks) {
      try {
        await this.#sendCustom(openid, chunk);
      } catch (error) {
        // One failed send, stop: the rest would fail identically, and three identical errors are
        // far harder to read than one useful one. The usual cause is an unverified account.
        this.#onLog(
          `the customer-service message did not go out: ${error instanceof Error ? error.message : String(error)}`,
        );
        return;
      }
    }
  }

  /**
   * The passive reply body.
   *
   * In encrypted mode the reply must be ciphertext too, and the outer `<xml>` carries
   * `Encrypt / MsgSignature / TimeStamp / Nonce` — not the message itself. `Nonce` echoes the
   * one on WeChat's URL (the docs say so), `TimeStamp` is the current time.
   */
  #replyXml(
    reply: string,
    openid: string,
    account: string,
    nonce: string,
  ): string {
    const time = Math.floor(Date.now() / 1000);
    // `ToUserName` and `FromUserName` are **swapped**: who it goes to, and who replies.
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
    // Keep only the recent batch: a "just seen or not" check, not history. WeChat's retries land
    // within seconds; 200 entries suffice, and Set keeps insertion order, so eviction is oldest.
    if (this.#seen.size > 200) {
      const oldest = this.#seen.values().next().value;
      if (oldest !== undefined) this.#seen.delete(oldest);
    }
  }

  /** Fetch `access_token`, cached. */
  async #getAccessToken(): Promise<string> {
    const secret = this.#appSecret;
    if (secret === undefined) {
      throw new Error(
        "no AppSecret — customer-service messages cannot be sent. (And only a verified account can call it.)",
      );
    }
    if (this.#accessToken !== undefined && Date.now() < this.#accessTokenUntil) {
      return this.#accessToken;
    }

    // Only one request under concurrency: WeChat **invalidates the old token the moment a new
    // one is fetched**, so two simultaneous "go fetch" calls kill the first token on the spot.
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
      throw new Error(`fetching access_token failed: ${body.errcode} ${body.errmsg}`);
    }

    this.#accessToken = body.access_token;
    // Invalidate 5 minutes early: fetching right at expiry risks the token dying mid-request.
    this.#accessTokenUntil =
      Date.now() + ((body.expires_in ?? 7200) - 300) * 1000;
    return body.access_token;
  }

  async #sendCustom(openid: string, content: string): Promise<void> {
    // One retry suffices, no loop: once `attempt` reaches 1, any error is thrown out.
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

      // 40001 / 42001 both mean "this token doesn't count". The cached one may have been fetched
      // elsewhere, or sits on the expiry edge — clear it and retry once; don't make users wait.
      if (
        attempt === 0 &&
        (body.errcode === 40001 || body.errcode === 42001)
      ) {
        this.#accessToken = undefined;
        this.#accessTokenUntil = 0;
        continue;
      }
      throw new Error(`the customer-service message was refused: ${body.errcode} ${body.errmsg}`);
    }
  }
}

/** Read the whole request body. Overlong: give up — WeChat messages are a few KB. */
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
