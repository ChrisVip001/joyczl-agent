import { JoyBridge } from "./bridge.ts";
import { TelegramAdapter } from "./telegram.ts";

const token = process.env["TELEGRAM_BOT_TOKEN"]?.trim();
if (token === undefined || token === "") {
  console.error(
    "没给 TELEGRAM_BOT_TOKEN。\n" +
      "找 @BotFather 建一个 bot 拿到 token，然后：\n" +
      "  TELEGRAM_BOT_TOKEN=123:abc TELEGRAM_ALLOW=@你的用户名 npm run start:telegram",
  );
  process.exit(1);
}

const allow = (process.env["TELEGRAM_ALLOW"] ?? "")
  .split(",")
  .map((entry) => entry.trim())
  .filter((entry) => entry !== "");

if (allow.length === 0) {
  // Usable without an allowlist, but that means "open this bot to everyone" — and it spends
  // your model quota. Such a default must announce itself, not wait for an incident.
  console.error(
    "注意：没设 TELEGRAM_ALLOW，这个 bot 谁都能用，消耗的是你的模型额度。\n" +
      "      只给自己用的话，填你的数字 id 或 @用户名。",
  );
}

const bridge = new JoyBridge();
const adapter = new TelegramAdapter(
  { token, allow, onLog: (text) => console.error(`[telegram] ${text}`) },
  async (conversation, text) => (await bridge.ask(conversation, text)).reply,
);

// Ctrl-C is the normal way to stop this process, not an abnormal exit: stop long polling first,
// then close app-server's stdin — it finishes writing queued frames and exits.
const shutdown = (): void => {
  adapter.stop();
  bridge.close();
};
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);

console.error("Joy 网关上来了，去 Telegram 里说句话试试。");
await adapter.run();
