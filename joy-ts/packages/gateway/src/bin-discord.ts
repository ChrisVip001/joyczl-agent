import { JoyBridge } from "./bridge.ts";
import { DiscordAdapter } from "./discord.ts";

const token = process.env["DISCORD_BOT_TOKEN"]?.trim();
if (token === undefined || token === "") {
  console.error(
    "DISCORD_BOT_TOKEN was not given.\n" +
      "Make an app at https://discord.com/developers/applications → Bot → Reset Token, then:\n" +
      "  DISCORD_BOT_TOKEN=… DISCORD_ALLOW=@your-login npm run start:discord",
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
    "Note: DISCORD_ALLOW is unset, so anyone can use this bot and spend your model quota.\n" +
      "      To keep it to yourself, put your numeric id or @login here.",
  );
}

// Without this notice, almost everyone gets stuck here on first run, and the error
// ("connection closed, code 4013") points nowhere near the real cause.
console.error(
  "Reminder: Discord withholds message bodies from bots by default. Go to the developer portal\n" +
    "      → Bot → Privileged Gateway Intents\n" +
    "      and turn Message Content Intent on, or the connection is closed outright (4013).",
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
  "The Joy gateway is up. Say something in a DM, or @ it in a channel — it will not answer in a "
  + "channel without an @.",
);
await adapter.run();
