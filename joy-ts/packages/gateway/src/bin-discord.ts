import { JoyBridge } from "./bridge.ts";
import { DiscordAdapter } from "./discord.ts";

const token = process.env["DISCORD_BOT_TOKEN"]?.trim();
if (token === undefined || token === "") {
  console.error(
    "没给 DISCORD_BOT_TOKEN。\n" +
      "去 https://discord.com/developers/applications 建一个应用 → Bot → Reset Token，然后：\n" +
      "  DISCORD_BOT_TOKEN=… DISCORD_ALLOW=@你的登录名 npm run start:discord",
  );
  process.exit(1);
}

const allow = (process.env["DISCORD_ALLOW"] ?? "")
  .split(",")
  .map((entry) => entry.trim())
  .filter((entry) => entry !== "");

if (allow.length === 0) {
  // Usable without an allowlist, but that means "open this bot to everyone" — and it spends
  // your model quota. Such a default must announce itself, not wait for an incident.
  console.error(
    "注意：没设 DISCORD_ALLOW，这个 bot 谁都能用，消耗的是你的模型额度。\n" +
      "      只给自己用的话，填你的数字 id 或 @登录名。",
  );
}

// Without this notice, almost everyone gets stuck here on first run, and the error
// ("connection closed, code 4013") points nowhere near the real cause.
console.error(
  "提醒：Discord 默认不给 bot 消息正文。要去开发者门户 → Bot → Privileged Gateway Intents\n" +
    "      把 Message Content Intent 打开，否则这条连接会被直接掐掉（4013）。",
);

const bridge = new JoyBridge();
const adapter = new DiscordAdapter(
  { token, allow, onLog: (text) => console.error(`[discord] ${text}`) },
  async (conversation, text) => (await bridge.ask(conversation, text)).reply,
);

// Ctrl-C is the normal way to stop this process, not an abnormal exit: close the WebSocket first,
// then app-server's stdin — it finishes writing queued frames and exits.
const shutdown = (): void => {
  adapter.stop();
  bridge.close();
};
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);

console.error(
  "Joy 网关上来了。私聊里说句话，或者在频道里 @ 它 —— 频道里不 @ 是不会接话的。",
);
await adapter.run();
