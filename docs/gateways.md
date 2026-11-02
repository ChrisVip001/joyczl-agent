# Chat gateways

English | [简体中文](gateways.zh.md)

Joy speaks four chat platforms. Each setup differs because the platforms do;
this page collects the real-world details. All gateways talk to a local
`joy app-server` and derive session ids from platform identity.

## Start a gateway

```sh
just gateway-telegram / gateway-discord / gateway-wechat / gateway-lark
```

## Telegram

The Telegram gateway needs a bot token (ask @BotFather):

```bash
TELEGRAM_BOT_TOKEN=123:abc TELEGRAM_ALLOW=@yourname just gateway-telegram
```

`TELEGRAM_ALLOW` is an allowlist (numeric ids or `@usernames`, comma
separated). **Unset, anyone can talk** — and it spends your model budget. Fine
for a private trial; always set it on a public bot.

The Discord gateway needs a bot token: create an app at
<https://discord.com/developers/applications> → Bot → Reset Token. **Also
enable `Message Content Intent` on the same page** — Discord withholds
message bodies by default and the connection is dropped otherwise (error
4013). Do both, invite the bot, then:

```bash
DISCORD_BOT_TOKEN=… DISCORD_ALLOW=@yourlogin just gateway-discord
```

`DISCORD_ALLOW` works like the Telegram allowlist: numeric id or `@login`,
comma separated, unset means anyone.

The four platforms differ because the platforms differ. Telegram is
**long-polling** (the gateway drives the offset itself; messages missed during
a reconnect arrive later). Discord is **one WebSocket** with its own
heartbeat / IDENTIFY / reconnect discipline — the adapter implements
"reconnect", **not RESUME**: messages during the gap are lost. Adding RESUME
means persisting `session_id` and `resume_gateway_url`, the only durable state
on that path. And in server channels the bot **only answers when mentioned** —
a Discord bot sees every message, and one that replies to everything gets
kicked the next day; DMs have no such problem.

**Note: this path has never touched real Discord.** It needs a real bot token
that local machines and CI don't have — `scripts/smoke-gateway.sh` proves the
gateway → app-server chain (session ids derived from platform identity, SSE
folded into a full reply). The adapter's own rules — heartbeat and sequencing,
reconnect re-handshake, mention detection, allowlist, message splitting, 429
backoff, a failed send not taking the gateway down — are pinned by unit tests;
"actually connects to Discord" is verified by whoever runs it first.

## WeChat

WeChat is the most different of the four: it has **no outbound connection** —
no long-polling, no WebSocket; it only pushes to an HTTP server you expose.
So this is the only gateway that needs "publicly reachable + one address":
configure a tunnel locally, and note WeChat only accepts ports 80 and 443
(terminate TLS on a reverse proxy in front, not in this process).

The credentials live in the official platform under "Settings & Development →
Basic Configuration → Server Configuration": Token, AppID, AppSecret,
EncodingAESKey; the callback URL is `http://your.domain/wechat`.

```bash
WECHAT_TOKEN=… WECHAT_APP_ID=wx… WECHAT_APP_SECRET=… just gateway-wechat
```

**The one critical rule here is five seconds.** After pushing a message WeChat
waits 5 seconds, then retries (three times total); after three failures the
user's message is dropped. A Joy turn — especially one with tool calls — is
far longer than 5 seconds, so the main path is not "reply passively" but:

1. On receipt, hand the reply work to Joy **and start a 4-second timer**
2. If it fits in 4 seconds and one message → reply passively, zero API calls
3. Otherwise → **immediately answer `success`** to dismiss WeChat, then push
   the reply through the customer-service message API

The 4 seconds leave headroom for the network and WeChat's side. The order is
not stylistic: reversed, the user receives three identical answers (WeChat
retried three times), so the adapter remembers the last 200 `MsgId`s and skips
retries.

**Two thresholds worth knowing up front — WeChat's, not ours:**

- The customer-service API requires a **verified** official account.
  Personal unverified subscription accounts keep getting `48001`; those can
  only use path 2 — fast answers reply, slow ones have no sequel.
- Customer-service messages are quota-bound: each user message buys
  **5 messages within 48 hours**, which is why "prefer passive reply over
  customer-service" has its own logic.

All three encryption modes are accepted (plain / compatible / safe) — inbound
bodies are normalized to plain XML, so the rest of the logic sees one shape.
Encryption uses `WECHAT_AES_KEY`; unset means plain mode. **Whatever mode
WeChat's side is configured for, configure the same here** — a mismatch shows
up as "URL verification failed". `packages/gateway/src/wechat-crypto.ts` is a
separate file of pure functions because it is the least "close enough is fine"
piece: one byte off and WeChat silently ignores you. The **ciphertext from the
official docs** is the test vector — it pins key derivation, CBC and IV,
32-byte padding, and plaintext layout in one case.

**Note: this path has never touched real WeChat.** It needs a real official
account and a public address, which local machines and CI don't have. But
everything except "WeChat actually pushed a message" verifies locally:
encryption matches the official vector; `scripts/smoke-gateway.sh` really
starts an HTTP server, really verifies the signature, and really prints the
response XML (session id derived from the openid, accepted by the server on
the spot); unit tests pin the three modes, the 4-second race, `MsgId`
deduplication, the allowlist, message splitting, and "customer-service
rejection does not take the gateway down" — the HTTP server is real, only the
two WeChat API addresses are stubbed. The last step belongs to whoever runs it
first.

## Lark / Feishu

Lark is yet another shape: it offers a **long connection**, so no public
address and no tunneling — but the direction flips: after the connection is
established **we must speak first** for the server to consider the line alive.
And because it separates "received" from "answered", there is no WeChat-style
5-second problem here: **ACK first, think slowly after**.

Credentials are in the developer console under "Credentials & Basic Info":
App ID, App Secret.

```bash
LARK_APP_ID=cli_… LARK_APP_SECRET=… LARK_ALLOW=ou_… just gateway-lark
```

`LARK_ALLOW` takes the **open_id** (the `ou_…` string), **not the App ID** —
the most commonly misfilled field; like the others, unset means anyone. For
the international Lark add `LARK_DOMAIN=lark`.

**Three console settings, any one missing shows up as "connected, forever
silent", with no error at all:**

1. Enable **bot capability**
2. Subscribe to `im.message.receive_v1`
3. Choose the **long connection** delivery method, not "push to your server"

The third is the nastiest: pick the other one and the connection still
establishes, zero events arrive, and the logs stay clean. `bin-lark.ts` lists
all three at startup.

Frames use Lark's private `pbbp2` encoding; `lark-proto.ts` hand-writes a
protobuf codec with no runtime dependency. One subtle, fatal detail: `logId`
is an integer **beyond 2^53** and the ACK must echo it verbatim — so it is
stored as BigInt. A `number` loses precision at the 16th digit, manifesting as
**sporadic non-acknowledgment**, the hardest class of bug. Large messages
arrive as fragments and are reassembled before processing (incomplete
fragments are ignored rather than half-parsed — parsing half a JSON only
yields "event is not JSON"). In groups the Discord rule applies: no mention,
no reply.

**Note: this path has never touched real Lark.** It needs a published
enterprise app that local machines and CI don't have.
`scripts/smoke-gateway.sh` stubs only the two remote addresses (fake socket +
fake HTTP); **everything in between runs for real**: real codecs (a frame it
encoded and cannot decode explodes on the spot), real ACK echo (including the
over-2^53 `logId`), real dedup and reassembly, a real app-server — session id
derived from the openid, accepted on the spot. The adapter's own rules are
pinned by unit tests; the last step belongs to whoever runs it first.
