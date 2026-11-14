import { JoyBridge } from "./bridge.ts";
import { WeChatAdapter } from "./wechat.ts";

/** The few without which nothing starts. When one is missing, say where to get it, not just the name. */
function required(name: string, hint: string): string {
  const value = process.env[name]?.trim();
  if (value === undefined || value === "") {
    console.error(`${name} was not given. ${hint}`);
    process.exit(1);
  }
  return value;
}

const token = required(
  "WECHAT_TOKEN",
  "The Token you filled in yourself under Settings & development → Basic configuration → "
  + "Server configuration.",
);
const appId = required(
  "WECHAT_APP_ID",
  "The account's AppID — the string starting with wx, right there on the same page.",
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
  // Usable unset, but that means "every follower can command it" — and it spends your model
  // quota. Such a default must announce itself, not wait for an incident.
  console.error(
    "Note: WECHAT_ALLOW is unset, so anyone who follows this account can use it and spend your\n" +
    "      model quota.\n" +
      "      To keep it to yourself, put your OpenID here (visible in the platform's user list).",
  );
}

if (appSecret === undefined) {
  // This isn't "an optional feature is off" but "half the messages will have no reply" — it must
  // be spelled out, or the symptom looks like Joy refusing to answer.
  console.error(
    "Note: WECHAT_APP_SECRET is unset. WeChat allows 5 seconds and then drops the user's line,\n" +
      "      and a Joy turn (especially with tool calls) usually takes far longer — so without it,\n" +
      "      most messages get no answer at all.\n" +
      "      Also, the customer-service API only answers for a **verified** account; on an unverified\n" +
    "      personal subscription account it stays closed.",
  );
}

if (aesKey !== undefined && aesKey.length !== 43) {
  console.error(
    `Reminder: WECHAT_AES_KEY is ${aesKey.length} characters, expected 43.` +
      "One character was probably lost or added while copying — then nothing decrypts at all.",
  );
}

console.error(
  `Reminder: WeChat wants a **publicly reachable** address, and only ports 80 and 443.\n` +
    `      A local run needs a tunnel first, then put the ${path} address into «Server configuration».`,
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

// Ctrl-C is the normal way to stop this process, not an abnormal exit: stop the server first
// (WeChat gets connection refused on its next request — exactly what we want it to learn), then
// close app-server's stdin — it finishes writing queued frames and exits.
const shutdown = (): void => {
  adapter.stop();
  bridge.close();
};
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);

console.error(
  `The Joy WeChat gateway is up, waiting for ${path} on port ${port}. Say something in the account.`,
);
await adapter.run();
