import { JoyBridge } from "./bridge.ts";
import { LarkAdapter } from "./lark.ts";

const appId = process.env["LARK_APP_ID"]?.trim();
const appSecret = process.env["LARK_APP_SECRET"]?.trim();
if (appId === undefined || appId === "" || appSecret === undefined || appSecret === "") {
  console.error(
    "LARK_APP_ID / LARK_APP_SECRET were not given.\n" +
      "Make a self-built app at https://open.feishu.cn/app → credentials, then:\n" +
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
    "Note: LARK_ALLOW is unset, so anyone can use this bot and spend your model quota.\n" +
      "      To keep it to yourself, put your open_id (starts with ou_…).",
  );
}

// Without this section, almost everyone gets stuck on first run — and there's **no error at all**:
// the connection is fine, pings are fine, logs are quiet, and messages just never arrive.
// Same family as Discord's Message Content Intent reminder.
console.error(
  "Reminder: for this app to receive messages, three things have to be done in the developer\n" +
    "      console; miss one and it is «connected but never reacts»:\n" +
    "      1. Events & callbacks → subscribe over «long connection» (not a webhook);\n" +
    "      2. add the event `im.message.receive_v1` (receive messages);\n" +
    "      3. add the permissions `im:message` (read) and `im:message:send_as_bot` (send as the app),\n" +
    "      then release a version.",
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
  "The Joy gateway is up. Say something in a direct chat, or @ it in a group — it will not answer "
  + "in a group without an @.",
);
await adapter.run();
