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
  // 不设白名单是能用的，但那是「把这个 bot 敞给所有人」的意思 ——
  // 而花的是你的模型额度。这种默认值必须自己说出来，不能等出事才发现。
  console.error(
    "注意：没设 DISCORD_ALLOW，这个 bot 谁都能用，消耗的是你的模型额度。\n" +
      "      只给自己用的话，填你的数字 id 或 @登录名。",
  );
}

// 这一步不说，绝大多数人第一次跑都会卡在这儿，而且报的错
// （「连接被关闭，代码 4013」）完全指不到真正的原因。
console.error(
  "提醒：Discord 默认不给 bot 消息正文。要去开发者门户 → Bot → Privileged Gateway Intents\n" +
    "      把 Message Content Intent 打开，否则这条连接会被直接掐掉（4013）。",
);

const bridge = new JoyBridge();
const adapter = new DiscordAdapter(
  { token, allow, onLog: (text) => console.error(`[discord] ${text}`) },
  async (conversation, text) => (await bridge.ask(conversation, text)).reply,
);

// Ctrl-C 是关这个进程的正常方式，不是异常退出：先把 WebSocket 关掉，
// 再关 app-server 的 stdin —— 它会写完排着的帧再退。
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
