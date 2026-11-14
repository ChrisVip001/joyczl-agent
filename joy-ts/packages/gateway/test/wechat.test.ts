import assert from "node:assert/strict";
import test from "node:test";

import { WeChatAdapter, MESSAGE_LIMIT, tag } from "../src/wechat.ts";
import {
  decryptMessage,
  encryptMessage,
  signatureOf,
} from "../src/wechat-crypto.ts";

const TOKEN = "joytest";
const APPID = "wxba5fad812f8e6fb9";
const AES_KEY = "A".repeat(43);
const ACCOUNT = "gh_abc123";

/** A fake WeChat API: knows only the token fetch and the customer-service send. */
class FakeApi {
  readonly sent: string[] = [];
  tokenRequests = 0;
  /** The next customer-service send is refused. What an unverified account gets: 48001. */
  #refuseNext = 0;
  /** The next send says "this token doesn't count" — happens a few times a day on real phones. */
  #expireNext = 0;

  refuseOnce(): void {
    this.#refuseNext += 1;
  }

  expireOnce(): void {
    this.#expireNext += 1;
  }

  readonly fetch = async (
    input: string | URL | Request,
    init?: RequestInit,
  ): Promise<Response> => {
    const url = String(input);
    if (url.includes("/cgi-bin/token")) {
      this.tokenRequests += 1;
      return Response.json({
        access_token: `tok-${this.tokenRequests}`,
        expires_in: 7200,
      });
    }

    if (this.#expireNext > 0) {
      this.#expireNext -= 1;
      return Response.json({ errcode: 40001, errmsg: "invalid credential" });
    }

    if (this.#refuseNext > 0) {
      this.#refuseNext -= 1;
      return Response.json({ errcode: 48001, errmsg: "api unauthorized" });
    }

    const body = JSON.parse(String(init?.body)) as {
      touser: string;
      text: { content: string };
    };
    this.sent.push(body.text.content);
    return Response.json({ errcode: 0, errmsg: "ok" });
  };
}

/** One text message pushed by WeChat. */
function incoming(openid: string, content: string, msgId: string): string {
  return (
    "<xml>" +
    `<ToUserName><![CDATA[${ACCOUNT}]]></ToUserName>` +
    `<FromUserName><![CDATA[${openid}]]></FromUserName>` +
    `<CreateTime>1713424427</CreateTime>` +
    `<MsgType><![CDATA[text]]></MsgType>` +
    `<Content><![CDATA[${content}]]></Content>` +
    `<MsgId>${msgId}</MsgId>` +
    "</xml>"
  );
}

const NONCE = "415670741";
const TIMESTAMP = "1714112445";

/** Query string for plain mode. */
function plainQuery(): string {
  return new URLSearchParams({
    signature: signatureOf(TOKEN, TIMESTAMP, NONCE),
    timestamp: TIMESTAMP,
    nonce: NONCE,
  }).toString();
}

/** Request body and query string for encrypted (safe) mode. */
function encrypted(xml: string): { body: string; query: string } {
  const encrypt = encryptMessage(xml, {
    encodingAesKey: AES_KEY,
    appId: APPID,
  });
  return {
    body:
      "<xml>" +
      `<ToUserName><![CDATA[${ACCOUNT}]]></ToUserName>` +
      `<Encrypt><![CDATA[${encrypt}]]></Encrypt>` +
      "</xml>",
    query: new URLSearchParams({
      // In safe mode this `signature` proves nothing and the docs say not to use it — a fake
      // value is filled here on purpose, proving the implementation really ignores it.
      signature: "deadbeef",
      timestamp: TIMESTAMP,
      nonce: NONCE,
      encrypt_type: "aes",
      msg_signature: signatureOf(TOKEN, TIMESTAMP, NONCE, encrypt),
    }).toString(),
  };
}

interface Harness {
  base: string;
  api: FakeApi;
  running: Promise<void>;
  logs: string[];
  asked: string[];
  stop: () => Promise<void>;
}

/** Start a real HTTP server (system-picked port) and tear it down before exit. */
async function start(
  options: {
    encodingAesKey?: string;
    appSecret?: string;
    allow?: string[];
    replyBudgetMs?: number;
    reply?: (text: string) => Promise<string> | string;
  } = {},
): Promise<Harness> {
  const api = new FakeApi();
  const logs: string[] = [];
  const asked: string[] = [];

  const adapter = new WeChatAdapter(
    {
      token: TOKEN,
      appId: APPID,
      encodingAesKey: options.encodingAesKey,
      appSecret: options.appSecret ?? "secret",
      port: 0,
      path: "/wechat",
      allow: options.allow,
      replyBudgetMs: options.replyBudgetMs,
      fetch: api.fetch,
      onLog: (text) => logs.push(text),
    },
    async (conversation, text) => {
      asked.push(`${conversation}|${text}`);
      return await (options.reply?.(text) ?? "the answer");
    },
  );

  const running = adapter.run();
  const deadline = Date.now() + 5000;
  while (adapter.port === 0) {
    if (Date.now() > deadline) throw new Error("the server never came up");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }

  return {
    base: `http://127.0.0.1:${adapter.port}/wechat`,
    api,
    running,
    logs,
    asked,
    stop: async () => {
      adapter.stop();
      await running;
    },
  };
}

/** Wait until the condition holds, or time out. */
async function until(
  what: string,
  ok: () => boolean,
  ms = 5000,
): Promise<void> {
  const deadline = Date.now() + ms;
  while (!ok()) {
    if (Date.now() > deadline) throw new Error(`waited in vain for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

async function post(base: string, query: string, body: string): Promise<string> {
  const response = await fetch(`${base}?${query}`, { method: "POST", body });
  return await response.text();
}

// ---- The server-configuration step --------------------------------------------------------

test("URL verification: with the right signature echostr goes back verbatim", async () => {
  const harness = await start();
  const query = new URLSearchParams({
    signature: signatureOf(TOKEN, TIMESTAMP, NONCE),
    timestamp: TIMESTAMP,
    nonce: NONCE,
    echostr: "1616140317555161061",
  });
  const response = await fetch(`${harness.base}?${query}`);
  assert.equal(response.status, 200);
  assert.equal(await response.text(), "1616140317555161061");
  await harness.stop();
});

test("URL verification: a wrong signature is 403, not «anything will do»", async () => {
  const harness = await start();
  const query = new URLSearchParams({
    signature: "0".repeat(40),
    timestamp: TIMESTAMP,
    nonce: NONCE,
    echostr: "1616140317555161061",
  });
  const response = await fetch(`${harness.base}?${query}`);
  assert.equal(response.status, 403);
  await harness.stop();
  assert.ok(
    harness.logs.some((line) => line.includes("URL verification failed")),
    "it has to leave a trace, or «verification failed» can only be guessed at",
  );
});

// ---- Fast answers: passive reply in place ----------------------------------------------------

test("answered quickly: straight back in the response, no API call at all", async () => {
  const harness = await start({ reply: () => "I have read it" });
  const body = await post(
    harness.base,
    plainQuery(),
    incoming("openid-1", "take a look for me", "1001"),
  );

  assert.match(body, /<Content><!\[CDATA\[I have read it\]\]><\/Content>/);
  // Who it goes to, and who replies — these two are **swapped**.
  assert.equal(tag(body, "ToUserName"), "openid-1");
  assert.equal(tag(body, "FromUserName"), ACCOUNT);
  assert.equal(tag(body, "MsgType"), "text");
  assert.deepEqual(harness.asked, ["wechat:openid-1|take a look for me"]);
  assert.equal(harness.api.sent.length, 0, "a passive reply means no call to the customer-service API");

  await harness.stop();
});

test("answered quickly but too long: passive is out, and the text goes out in several", async () => {
  const long = "x".repeat(MESSAGE_LIMIT + 50);
  const harness = await start({ reply: () => long });
  const body = await post(
    harness.base,
    plainQuery(),
    incoming("openid-1", "tell me in detail", "1002"),
  );

  // A passive reply is **one** message only, so a two-chunk answer must go via customer service.
  assert.equal(body, "success");
  await until("the customer-service messages go out", () => harness.api.sent.length === 2);
  assert.equal(harness.api.sent[0]!.length, MESSAGE_LIMIT);
  assert.equal(harness.api.sent.join(""), long);

  await harness.stop();
});

// ---- Slow answers: send WeChat away first, then push customer-service messages --------------------------------------

test("answered slowly: send success to get WeChat off the line, then push the answer back", async () => {
  const harness = await start({
    replyBudgetMs: 50,
    reply: async () => {
      await new Promise((resolve) => setTimeout(resolve, 150));
      return "thought it over";
    },
  });

  const body = await post(
    harness.base,
    plainQuery(),
    incoming("openid-1", "work something out for me", "1003"),
  );

  // WeChat waits only 5 seconds. Joy is still running; an answer must already be given here —
  // otherwise it pushes the same request twice more, and the user sees three identical replies.
  assert.equal(body, "success");
  await until("the customer-service message goes back", () => harness.api.sent.length === 1);
  assert.deepEqual(harness.api.sent, ["thought it over"]);
  assert.equal(harness.api.tokenRequests, 1);

  await harness.stop();
});

test("WeChat pushes the same message again: Joy is not asked twice and nothing is resent", async () => {
  const harness = await start({
    replyBudgetMs: 50,
    reply: async () => {
      await new Promise((resolve) => setTimeout(resolve, 150));
      return "thought it over";
    },
  });

  const query = plainQuery();
  const xml = incoming("openid-1", "work something out for me", "1004");
  assert.equal(await post(harness.base, query, xml), "success");
  // Second and third pushes: what WeChat does when its 5 seconds pass without a reply.
  assert.equal(await post(harness.base, query, xml), "success");
  assert.equal(await post(harness.base, query, xml), "success");

  await until("the customer-service message goes back", () => harness.api.sent.length === 1);
  assert.equal(harness.asked.length, 1, "one message may reach Joy only once");
  assert.ok(harness.logs.some((line) => line.includes("again")));

  await harness.stop();
});

test("the customer-service message is refused (an unverified account): say so, the gateway must not fall over", async () => {
  const harness = await start({
    replyBudgetMs: 50,
    reply: async () => {
      await new Promise((resolve) => setTimeout(resolve, 150));
      return "thought it over";
    },
  });
  harness.api.refuseOnce();

  assert.equal(
    await post(harness.base, plainQuery(), incoming("openid-1", "are you there", "1005")),
    "success",
  );

  await until("the log says why", () =>
    harness.logs.some((line) => line.includes("did not go out")),
  );
  assert.ok(harness.logs.some((line) => line.includes("48001")));

  await harness.stop();
});

test("the customer-service API says «this token no longer counts»: fetch another and come back, do not leave the user waiting", async () => {
  // It must take the customer-service path — a fast answer never fetches a token at all.
  const harness = await start({
    replyBudgetMs: 50,
    reply: async () => {
      await new Promise((resolve) => setTimeout(resolve, 150));
      return "the answer";
    },
  });
  harness.api.expireOnce();

  assert.equal(
    await post(harness.base, plainQuery(), incoming("openid-1", "are you there", "1009")),
    "success",
  );

  await until("the answer is pushed back", () => harness.api.sent.length === 1);
  assert.equal(harness.api.tokenRequests, 2, "after the first refusal it fetches again");
  assert.deepEqual(harness.api.sent, ["the answer"]);

  await harness.stop();
});

// ---- Allowlist ------------------------------------------------------------------

test("outside the allowlist: no answer, and no telling them they lack permission", async () => {
  const harness = await start({ allow: ["openid-yes"] });
  const body = await post(
    harness.base,
    plainQuery(),
    incoming("openid-no", "hello", "1010"),
  );

  assert.equal(body, "success");
  assert.deepEqual(harness.asked, [], "someone off the list must not spend your model quota");
  assert.ok(harness.logs.some((line) => line.includes("outside the allowlist")));

  await harness.stop();
});

// ---- Encrypted mode ----------------------------------------------------------------

test("encrypted mode: it opens the message, and the reply goes back encrypted too", async () => {
  const harness = await start({ encodingAesKey: AES_KEY, reply: () => "got it" });
  const { body, query } = encrypted(incoming("openid-1", "hello", "1011"));
  const reply = await post(harness.base, query, body);

  const encrypt = tag(reply, "Encrypt");
  assert.notEqual(encrypt, "", "in encrypted mode the reply has to be ciphertext");
  // Decrypt the reply — pinning both "the reply's plaintext layout" and "the AppID matches".
  const inner = decryptMessage(encrypt, {
    encodingAesKey: AES_KEY,
    appId: APPID,
  });
  assert.match(inner, /<Content><!\[CDATA\[got it\]\]><\/Content>/);
  assert.equal(
    tag(reply, "MsgSignature"),
    signatureOf(TOKEN, tag(reply, "TimeStamp"), NONCE, encrypt),
  );

  await harness.stop();
});

test("tampered ciphertext: 403, and it is not used to guess", async () => {
  const harness = await start({ encodingAesKey: AES_KEY });
  const { body, query } = encrypted(incoming("openid-1", "hello", "1012"));
  const tampered = query.replace(
    /msg_signature=[^&]+/,
    `msg_signature=${"0".repeat(40)}`,
  );

  const response = await fetch(`${harness.base}?${tampered}`, {
    method: "POST",
    body,
  });
  assert.equal(response.status, 403);
  assert.deepEqual(harness.asked, []);

  await harness.stop();
});

test("the WeChat side is in encrypted mode with no key given: 403, and the log says why", async () => {
  const harness = await start();
  const { body, query } = encrypted(incoming("openid-1", "hello", "1013"));

  const response = await fetch(`${harness.base}?${query}`, {
    method: "POST",
    body,
  });
  assert.equal(response.status, 403);
  assert.ok(harness.logs.some((line) => line.includes("WECHAT_AES_KEY")));

  await harness.stop();
});


