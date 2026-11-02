import { JoyBridge } from "./bridge.ts";
import { LarkAdapter } from "./lark.ts";

const appId = process.env["LARK_APP_ID"]?.trim();
const appSecret = process.env["LARK_APP_SECRET"]?.trim();
if (appId === undefined || appId === "" || appSecret === undefined || appSecret === "") {
  console.error(
    "没给 LARK_APP_ID / LARK_APP_SECRET。\n" +
      "去 https://open.feishu.cn/app 建一个企业自建应用 → 凭证与基础信息，然后：\n" +
      "  LARK_APP_ID=cli_… LARK_APP_SECRET=… npm run start:lark",
  );
  process.exit(1);
}

// 白名单填的是 `open_id`（`ou_…`），不是手机号也不是邮箱 —— 因为飞书推过来的
// 事件里只有 open_id。拿别的填会一个人都匹配不上，表现是「它不理我」。
const allow = (process.env["LARK_ALLOW"] ?? "")
  .split(",")
  .map((entry) => entry.trim())
  .filter((entry) => entry !== "");

if (allow.length === 0) {
  // 跟 Discord 那条同一个理由：不设白名单是「把这个 bot 敞给公司里所有人」，
  // 而花的是你的模型额度。这种默认值必须自己说出来，不能等出事才发现。
  console.error(
    "注意：没设 LARK_ALLOW，这个 bot 谁都能用，消耗的是你的模型额度。\n" +
      "      只给自己用的话，填你的 open_id（ou_… 开头）。",
  );
}

// 这一段不说，绝大多数人第一次跑都会卡在这儿 —— 而且它**连报错都没有**：
// 连接是好的、心跳是好的、日志上一片安静，就是永远不来消息。
// 这跟 Discord 那句 Message Content Intent 的提醒是同一类东西。
console.error(
  "提醒：这个应用要能收到消息，得在开发者后台做三件事，缺一件都会「连上了但永远没反应」：\n" +
    "      1. 事件与回调 → 订阅方式选「使用长连接接收事件」（不是 webhook）；\n" +
    "      2. 添加事件 `im.message.receive_v1`（接收消息）；\n" +
    "      3. 权限里加上 `im:message`（读消息）和 `im:message:send_as_bot`（以应用身份发消息），然后发版。",
);

const bridge = new JoyBridge();
const adapter = new LarkAdapter(
  {
    appId,
    appSecret,
    allow,
    // 国际版 Lark 走另一个域名，两个域名的接口完全不通用。
    lark: process.env["LARK_DOMAIN"]?.trim().toLowerCase() === "lark",
    onLog: (text) => console.error(`[lark] ${text}`),
  },
  async (conversation, text) => (await bridge.ask(conversation, text)).reply,
);

// Ctrl-C 是关这个进程的正常方式，不是异常退出：先关长连接，再关 app-server 的
// stdin —— 它会写完排着的帧再退。
const shutdown = (): void => {
  adapter.stop();
  bridge.close();
};
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);

console.error(
  "Joy 网关上来了。单聊里说句话，或者在群里 @ 它 —— 群里不 @ 是不会接话的。",
);
await adapter.run();
