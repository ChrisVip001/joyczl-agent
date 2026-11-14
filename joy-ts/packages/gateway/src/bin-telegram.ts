import { JoyBridge } from "./bridge.ts";
import { TelegramAdapter } from "./telegram.ts";

const token = process.env["TELEGRAM_BOT_TOKEN"]?.trim();
if (token === undefined || token === "") {
  console.error(
    "TELEGRAM_BOT_TOKEN was not given.\n" +
      "Ask @BotFather for a bot and its token, then:\n" +
      "  TELEGRAM_BOT_TOKEN=123:abc TELEGRAM_ALLOW=@your-username npm run start:telegram",
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
    "Note: TELEGRAM_ALLOW is unset, so anyone can use this bot and spend your model quota.\n" +
      "      To keep it to yourself, put your numeric id or @username here.",
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

console.error("The Joy gateway is up; say something in Telegram.");
await adapter.run();
