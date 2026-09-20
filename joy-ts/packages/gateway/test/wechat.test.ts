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

/** 假的微信 API：只认取 token 和发客服消息这两个地址。 */
class FakeApi {
  readonly sent: string[] = [];
  tokenRequests = 0;
  /** 下一次发客服消息被拒。未认证的号就是这么回事：48001。 */
  #refuseNext = 0;
  /** 下一次发客服消息说「你这把 token 不算数了」—— 真机上一天总要遇上几次。 */
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

/** 微信推过来的一条文本消息。 */
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

/** 明文模式的查询串。 */
function plainQuery(): string {
  return new URLSearchParams({
    signature: signatureOf(TOKEN, TIMESTAMP, NONCE),
    timestamp: TIMESTAMP,
    nonce: NONCE,
  }).toString();
}

/** 加密模式（安全模式）的请求体与查询串。 */
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
      // 安全模式下这个 `signature` 是给不了什么的，文档也说别用它 ——
      // 这里故意填一串假的，证明实现确实没看它。
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

/** 起一个真的 HTTP 服务器（端口让系统挑），并在它退出前收尾。 */
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
      return await (options.reply?.(text) ?? "答复");
    },
  );

  const running = adapter.run();
  const deadline = Date.now() + 5000;
  while (adapter.port === 0) {
    if (Date.now() > deadline) throw new Error("服务器没起来");
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

/** 一直等到条件成立，或者超时。 */
async function until(
  what: string,
  ok: () => boolean,
  ms = 5000,
): Promise<void> {
  const deadline = Date.now() + ms;
  while (!ok()) {
    if (Date.now() > deadline) throw new Error(`没等到：${what}`);
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

async function post(base: string, query: string, body: string): Promise<string> {
  const response = await fetch(`${base}?${query}`, { method: "POST", body });
  return await response.text();
}

// ---- 服务器配置那一步 --------------------------------------------------------

test("验证 URL：签名对就把 echostr 原样回过去", async () => {
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

test("验证 URL：签名不对就 403，不是「随便给点什么都行」", async () => {
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
    harness.logs.some((line) => line.includes("URL 验证没通过")),
    "得留个痕，不然「验证失败」这句只能靠猜",
  );
});

// ---- 答得快：就地被动回复 ----------------------------------------------------

test("答得快：直接在响应里回，一次 API 都不调", async () => {
  const harness = await start({ reply: () => "我看过了" });
  const body = await post(
    harness.base,
    plainQuery(),
    incoming("openid-1", "帮我看下", "1001"),
  );

  assert.match(body, /<Content><!\[CDATA\[我看过了\]\]><\/Content>/);
  // 回给谁、谁回的 —— 这两个是**对调**的。
  assert.equal(tag(body, "ToUserName"), "openid-1");
  assert.equal(tag(body, "FromUserName"), ACCOUNT);
  assert.equal(tag(body, "MsgType"), "text");
  assert.deepEqual(harness.asked, ["wechat:openid-1|帮我看下"]);
  assert.equal(harness.api.sent.length, 0, "走被动回复就不该再去调客服接口");

  await harness.stop();
});

test("答得快但回答很长：就不走被动了，那段要切成几条", async () => {
  const long = "字".repeat(MESSAGE_LIMIT + 50);
  const harness = await start({ reply: () => long });
  const body = await post(
    harness.base,
    plainQuery(),
    incoming("openid-1", "详细讲讲", "1002"),
  );

  // 被动回复只能回**一条**，所以切成两条的回答只能走客服消息。
  assert.equal(body, "success");
  await until("客服消息发出去", () => harness.api.sent.length === 2);
  assert.equal(harness.api.sent[0]!.length, MESSAGE_LIMIT);
  assert.equal(harness.api.sent.join(""), long);

  await harness.stop();
});

// ---- 答得慢：先打发走微信，再推客服消息 --------------------------------------

test("答得慢：先回 success 把微信打发走，再把答复推回去", async () => {
  const harness = await start({
    replyBudgetMs: 50,
    reply: async () => {
      await new Promise((resolve) => setTimeout(resolve, 150));
      return "想好了";
    },
  });

  const body = await post(
    harness.base,
    plainQuery(),
    incoming("openid-1", "帮我算个东西", "1003"),
  );

  // 微信只等 5 秒。Joy 那边还在跑，这边必须已经给了交代 —— 否则它会
  // 把同一条再推两遍，用户看到的就是三份一模一样的回答。
  assert.equal(body, "success");
  await until("客服消息推回去", () => harness.api.sent.length === 1);
  assert.deepEqual(harness.api.sent, ["想好了"]);
  assert.equal(harness.api.tokenRequests, 1);

  await harness.stop();
});

test("微信把同一条再推一遍：不重问 Joy，也不重发", async () => {
  const harness = await start({
    replyBudgetMs: 50,
    reply: async () => {
      await new Promise((resolve) => setTimeout(resolve, 150));
      return "想好了";
    },
  });

  const query = plainQuery();
  const xml = incoming("openid-1", "帮我算个东西", "1004");
  assert.equal(await post(harness.base, query, xml), "success");
  // 第二遍、第三遍：微信那 5 秒没等到答复时就是这么干的。
  assert.equal(await post(harness.base, query, xml), "success");
  assert.equal(await post(harness.base, query, xml), "success");

  await until("客服消息推回去", () => harness.api.sent.length === 1);
  assert.equal(harness.asked.length, 1, "同一条消息只能问 Joy 一次");
  assert.ok(harness.logs.some((line) => line.includes("又")));

  await harness.stop();
});

test("客服消息被拒（未认证的号）：说清楚，网关不能跟着倒", async () => {
  const harness = await start({
    replyBudgetMs: 50,
    reply: async () => {
      await new Promise((resolve) => setTimeout(resolve, 150));
      return "想好了";
    },
  });
  harness.api.refuseOnce();

  assert.equal(
    await post(harness.base, plainQuery(), incoming("openid-1", "在吗", "1005")),
    "success",
  );

  await until("日志里说清原因", () =>
    harness.logs.some((line) => line.includes("客服消息没发出去")),
  );
  assert.ok(harness.logs.some((line) => line.includes("48001")));

  await harness.stop();
});

test("客服接口说「这把 token 不算数了」：自己重取一把再来，别让用户白等", async () => {
  // 得让它走客服消息那条路 —— 答得快的话根本不会去取 token。
  const harness = await start({
    replyBudgetMs: 50,
    reply: async () => {
      await new Promise((resolve) => setTimeout(resolve, 150));
      return "答复";
    },
  });
  harness.api.expireOnce();

  assert.equal(
    await post(harness.base, plainQuery(), incoming("openid-1", "在吗", "1009")),
    "success",
  );

  await until("答复推回去", () => harness.api.sent.length === 1);
  assert.equal(harness.api.tokenRequests, 2, "第一次被拒之后要重取");
  assert.deepEqual(harness.api.sent, ["答复"]);

  await harness.stop();
});

// ---- 白名单 ------------------------------------------------------------------

test("不在白名单里：不搭理，也不告诉他自己没权限", async () => {
  const harness = await start({ allow: ["openid-yes"] });
  const body = await post(
    harness.base,
    plainQuery(),
    incoming("openid-no", "你好", "1010"),
  );

  assert.equal(body, "success");
  assert.deepEqual(harness.asked, [], "不在名单里的人不该花掉你的模型额度");
  assert.ok(harness.logs.some((line) => line.includes("不在白名单")));

  await harness.stop();
});

// ---- 加密模式 ----------------------------------------------------------------

test("加密模式：解得开，回包也是加密的", async () => {
  const harness = await start({ encodingAesKey: AES_KEY, reply: () => "收到" });
  const { body, query } = encrypted(incoming("openid-1", "你好", "1011"));
  const reply = await post(harness.base, query, body);

  const encrypt = tag(reply, "Encrypt");
  assert.notEqual(encrypt, "", "加密模式下回包得是密文");
  // 把回包解回来 —— 这一步同时钉住了「回包的明文结构」和「AppID 对得上」。
  const inner = decryptMessage(encrypt, {
    encodingAesKey: AES_KEY,
    appId: APPID,
  });
  assert.match(inner, /<Content><!\[CDATA\[收到\]\]><\/Content>/);
  assert.equal(
    tag(reply, "MsgSignature"),
    signatureOf(TOKEN, tag(reply, "TimeStamp"), NONCE, encrypt),
  );

  await harness.stop();
});

test("密文被改过：403，不拿它去猜", async () => {
  const harness = await start({ encodingAesKey: AES_KEY });
  const { body, query } = encrypted(incoming("openid-1", "你好", "1012"));
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

test("微信那边选了加密模式，但没给密钥：403 并说清是为什么", async () => {
  const harness = await start();
  const { body, query } = encrypted(incoming("openid-1", "你好", "1013"));

  const response = await fetch(`${harness.base}?${query}`, {
    method: "POST",
    body,
  });
  assert.equal(response.status, 403);
  assert.ok(harness.logs.some((line) => line.includes("WECHAT_AES_KEY")));

  await harness.stop();
});


