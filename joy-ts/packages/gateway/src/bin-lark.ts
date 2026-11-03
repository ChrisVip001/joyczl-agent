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

// The allowlist takes `open_id`s (`ou_…`), not phone numbers or emails — Lark events carry only
// the open_id. Anything else matches nobody, and the symptom is "it ignores me".
const allow = (process.env["LARK_ALLOW"] ?? "")
  .split(",")
  .map((entry) => entry.trim())
  .filter((entry) => entry !== "");

if (allow.length === 0) {
  // Same reason as the Discord one: no allowlist means "open this bot to everyone in the
  // company", and it spends your model quota. The default must announce itself, not wait.
  console.error(
    "注意：没设 LARK_ALLOW，这个 bot 谁都能用，消耗的是你的模型额度。\n" +
      "      只给自己用的话，填你的 open_id（ou_… 开头）。",
  );
}

// Without this section, almost everyone gets stuck on first run — and there's **no error at all**:
// the connection is fine, pings are fine, logs are quiet, and messages just never arrive.
// Same family as Discord's Message Content Intent reminder.
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
    // International Lark uses another domain; the two domains' APIs are entirely incompatible.
    lark: process.env["LARK_DOMAIN"]?.trim().toLowerCase() === "lark",
    onLog: (text) => console.error(`[lark] ${text}`),
  },
  async (conversation, text) => (await bridge.ask(conversation, text)).reply,
);

// Ctrl-C is the normal way to stop this process, not an abnormal exit: close the long connection
// first, then app-server's stdin — it finishes writing queued frames and exits.
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
