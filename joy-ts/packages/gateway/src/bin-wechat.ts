import { JoyBridge } from "./bridge.ts";
import { WeChatAdapter } from "./wechat.ts";

/** 少一个就跑不起来的那几个。缺了就说清去哪儿拿，别只报个变量名。 */
function required(name: string, hint: string): string {
  const value = process.env[name]?.trim();
  if (value === undefined || value === "") {
    console.error(`${name} 没给。${hint}`);
    process.exit(1);
  }
  return value;
}

const token = required(
  "WECHAT_TOKEN",
  "它在公众平台的「设置与开发 → 基本配置 → 服务器配置」里，你自己填的那个 Token。",
);
const appId = required(
  "WECHAT_APP_ID",
  "公众号的 AppID，wx 开头那串，同一页面上就有。",
);
const appSecret = process.env["WECHAT_APP_SECRET"]?.trim() || undefined;
const aesKey = process.env["WECHAT_AES_KEY"]?.trim() || undefined;
const path = process.env["WECHAT_PATH"]?.trim() || "/wechat";
const port = Number(process.env["WECHAT_PORT"] ?? "8080");

const allow = (process.env["WECHAT_ALLOW"] ?? "")
  .split(",")
  .map((entry) => entry.trim())
  .filter((entry) => entry !== "");

if (allow.length === 0) {
  // 不设是能用的，但那是「让所有关注者都能使唤它」的意思 —— 而花的是你的
  // 模型额度。这种默认值必须自己说出来，不能等出事才发现。
  console.error(
    "注意：没设 WECHAT_ALLOW，关注这个号的人谁都能用，消耗的是你的模型额度。\n" +
      "      只给自己用的话，填你的 OpenID（在公众平台的用户列表里能看到）。",
  );
}

if (appSecret === undefined) {
  // 这条不是「可选功能没开」，而是「一半的消息会没有下文」—— 得说清楚，
  // 不然现象看着像 Joy 不回话。
  console.error(
    "注意：没设 WECHAT_APP_SECRET。微信只给 5 秒，超了它就把用户那句丢掉，\n" +
      "      而 Joy 跑一轮（尤其带工具调用）通常远超 5 秒 —— 所以没有它的话，\n" +
      "      绝大多数消息都不会有回答。\n" +
      "      另外客服接口只有**认证过的**公众号能调，个人未认证订阅号开了也调不通。",
  );
}

if (aesKey !== undefined && aesKey.length !== 43) {
  console.error(
    `提醒：WECHAT_AES_KEY 是 ${aesKey.length} 个字符，应该是 43 个。` +
      "多半是复制的时候少了或多了一位 —— 那样一条消息都解不开。",
  );
}

console.error(
  `提醒：微信要一个**公网可达**的地址，而且只认 80 和 443 端口。\n` +
    `      本地跑得先配内网穿透，然后把 ${path} 那个地址填进「服务器配置」。`,
);

const bridge = new JoyBridge();
const adapter = new WeChatAdapter(
  {
    token,
    appId,
    appSecret,
    encodingAesKey: aesKey,
    port,
    path,
    allow,
    onLog: (text) => console.error(`[wechat] ${text}`),
  },
  async (conversation, text) => (await bridge.ask(conversation, text)).reply,
);

// Ctrl-C 是关这个进程的正常方式，不是异常退出：先停掉服务器（微信会在
// 下一次请求上收到连接被拒，这正是我们希望它知道的），再关 app-server 的
// stdin —— 它会写完排着的帧再退。
const shutdown = (): void => {
  adapter.stop();
  bridge.close();
};
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);

console.error(
  `Joy 微信网关上来了，在 ${port} 端口等 ${path}。在公众号里说句话试试。`,
);
await adapter.run();
