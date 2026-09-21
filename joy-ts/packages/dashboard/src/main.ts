// 驾驶舱的主程序：开机把当前会话的历史拉回来，然后等着说话。
//
// 页面本身**没有状态**：会话、记忆、配置都在 Rust 那边（app-server 背后是
// state.db）。这里只有「在看哪个会话」和「这一轮正在流」这两个临时的视图状态，
// 刷新一下就回到默认。也就是说，浏览器崩了、标签页关了，什么都不丢 ——
// 这是这套分层最值钱的地方。

import type { TurnCompletedNotification } from "./protocol";
import { fetchDashboard, fetchMessages, respondApproval, talk } from "./api";
import {
  chip,
  el,
  failure,
  failureOf,
  formatMs,
  formatStamp,
  messageRow,
  muted,
  renderMemory,
  renderOverview,
  turnFooter,
} from "./render";

/**
 * 现在在看哪个会话。
 *
 * 会话不是存在浏览器里的东西，只是 chat_log 上的一个标签 —— 所以「切换」
 * 就是换这个变量、再重新拉一遍历史，没有别的。
 * 默认会话是好事：终端里的 `joy` 说的也是这一条线，接得上。
 */
let current = "default";

/** 往上翻旧账的游标。`null` = 已经翻到最早了。 */
let olderCursor: string | null = null;

/** 面板多久自己刷一次。都是本地 RPC，几微秒，不心疼。 */
const POLL_MS = 15_000;

const dom = {
  messages: must("messages"),
  composer: must<HTMLFormElement>("composer"),
  input: must<HTMLTextAreaElement>("input"),
  send: must<HTMLButtonElement>("send"),
  refresh: must<HTMLButtonElement>("refresh"),
  older: must<HTMLButtonElement>("older"),
  overview: must("overview"),
  memory: must("memory"),
  model: must("model"),
  home: must("home"),
  session: must("session"),
  stamp: must("stamp"),
};

let busy = false;

async function boot(): Promise<void> {
  dom.refresh.addEventListener("click", () => void refresh());
  dom.older.addEventListener("click", () => void older());

  dom.composer.addEventListener("submit", (event) => {
    event.preventDefault();
    void submit();
  });

  dom.input.addEventListener("keydown", (event) => {
    // Enter 发送，Shift+Enter 换行 —— 聊天框的老规矩。
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void submit();
    }
  });

  setInterval(() => {
    // 正在流的时候别刷：面板的数字会跳，但气泡里的字更重要。
    if (!busy) void refresh();
  }, POLL_MS);

  await refresh();
  await open(current);
  dom.input.focus();
}

/** 把一屏数据画上去。 */
async function refresh(): Promise<void> {
  try {
    const data = await fetchDashboard();
    dom.overview.replaceChildren(renderOverview(data, current, (id) => void open(id)));
    dom.memory.replaceChildren(renderMemory(data));
    dom.model.textContent = `${data.config.provider} · ${data.config.model}`;
    dom.home.textContent = data.config.home;
    dom.home.title = data.config.home;
    dom.stamp.textContent = formatStamp(data.generatedAt);
  } catch (error) {
    // 拉不到数据是常态（app-server 刚起来、进程挂了），
    // 所以显示成一件事，而不是抛到控制台里没人看见。
    dom.overview.replaceChildren(failureOf(error));
  }
}

async function submit(): Promise<void> {
  const text = dom.input.value.trim();
  if (text === "" || busy) return;
  dom.input.value = "";
  await send(text);
}

/** 说一句，把这一轮的过程画出来。 */
async function send(text: string): Promise<void> {
  setBusy(true);

  dom.messages.append(el("div", { class: "msg me" }, el("div", { class: "bubble", text })));

  const trace = el("div", { class: "trace" });
  const bubble = el("div", { class: "bubble streaming" });
  const foot = el("div", { class: "foot" });
  dom.messages.append(el("div", { class: "msg joy" }, trace, bubble, foot));
  scrollToEnd();

  let streamed = "";
  let completed: TurnCompletedNotification | null = null;

  try {
    for await (const notification of talk({ sessionId: current, message: text, stream: true })) {
      switch (notification.type) {
        case "turnStarted":
          break;

        case "textDelta":
          streamed += notification.delta;
          bubble.textContent = streamed;
          scrollToEnd();
          break;

        case "gateDecided":
          // 门决定要不要翻记忆。看得到这一句，就明白「它为什么记得/没记得」。
          trace.append(
            chip(
              notification.decision.decision === "retrieve"
                ? `检索「${notification.decision.query ?? ""}」`
                : `跳过记忆（${notification.decision.reason}）`,
              "gate",
            ),
          );
          break;

        case "approvalRequested": {
          // 要执行许可：给了按钮，点了才动 —— **不点就是拒绝**（默认拒绝）。
          const row = el("div", { class: "approval" });
          row.append(
            chip(`需要批准 · ${notification.tool}`, "gate"),
            el("code", { class: "cmd", text: notification.argsPreview }),
          );
          const settle = (approved: boolean, remember: boolean) => {
            row.querySelectorAll("button").forEach((button) => {
              (button as HTMLButtonElement).disabled = true;
            });
            void respondApproval({
              turnId: notification.turnId,
              requestId: notification.requestId,
              approved,
              remember,
            })
              .then((response) => {
                row.append(
                  chip(
                    response.accepted
                      ? approved
                        ? remember
                          ? "已允许，并记住这条命令"
                          : "已允许"
                        : "已拒绝"
                      : "回答得太晚，那一轮已经不等了",
                    "note",
                  ),
                );
              })
              .catch((error: unknown) => {
                row.append(chip(`回答没送出去：${String(error)}`, "note"));
              });
          };
          const allow = el("button", { class: "approve", text: "允许" });
          const always = el("button", { class: "approve", text: "允许并记住" });
          const deny = el("button", { class: "deny", text: "拒绝" });
          allow.addEventListener("click", () => settle(true, false));
          always.addEventListener("click", () => settle(true, true));
          deny.addEventListener("click", () => settle(false, false));
          row.append(allow, always, deny);
          trace.append(row);
          break;
        }

        case "retry":
          // 重试从不静默：看到这一句就知道「刚才那几秒不是卡住」。
          trace.append(
            chip(
              `重试第 ${notification.attempt} 次（${notification.reason}，${formatMs(
                notification.delayMs,
              )} 后）`,
              "gate",
            ),
          );
          break;

        case "toolStarted":
          trace.append(chip(`工具 · ${notification.tool}`, "tool"));
          break;

        case "toolCompleted":
          trace.append(
            chip(
              `工具 · ${notification.tool} ${
                notification.status === "ok" ? "好了" : "出错"
              }${notification.durationMs === null ? "" : ` ${formatMs(notification.durationMs)}`}`,
              "tool",
            ),
          );
          break;

        case "consolidationCompleted":
          if (notification.newFacts > 0) {
            trace.append(chip(`记住了 ${notification.newFacts} 条新事实`, "note"));
            void refresh(); // 记忆面板立刻跟上，不用等下一次轮询
          }
          break;

        case "graphStarted":
          trace.append(chip(`工作流 ${notification.workflow}`, "graph"));
          break;

        case "graphNodeStarted":
          trace.append(chip(`${notification.node} #${notification.visit}`, "graph"));
          break;

        case "graphNodeEnded":
          trace.append(chip(`${notification.node} ${formatMs(notification.ms)}`, "graph"));
          break;

        case "graphEnded":
          trace.append(
            chip(`工作流 ${notification.workflow} · ${notification.path.length} 步`, "graph"),
          );
          break;

        case "turnCompleted":
          completed = notification;
          break;

        case "error":
          trace.append(failure(notification));
          break;
      }
    }
  } catch (error) {
    trace.append(failureOf(error));
  } finally {
    // 收尾放在 finally：中途出错也要把「正在写」的状态清掉，
    // 否则那根闪动的光标会一直留在气泡上。
    bubble.classList.remove("streaming");

    if (completed !== null) {
      // 用 `reply` 覆盖流式攒出来的字：增量可能缺（背压、断线），
      // 而 reply 是 app-server 给的权威结果。
      bubble.textContent = completed.reply === "" ? "（这一轮没有回复）" : completed.reply;
      foot.textContent = turnFooter(completed);
    } else if (streamed !== "") {
      foot.textContent = "这一轮没有收到结尾 —— 上面的回复可能被截断了";
    } else {
      bubble.textContent = "（这一轮没有回复）";
    }

    scrollToEnd();
    setBusy(false);
    // 这一轮让会话的消息数、最后活跃时间都变了，顺手让面板跟上。
    void refresh();
    dom.input.focus();
  }
}

/**
 * 切到某个会话：换名字、把历史拉回来、准备在那儿说话。
 *
 * 正在流的时候不给切 —— 那一轮的字节还在往气泡里灌，换了会话就灌到别人家去了。
 */
async function open(sessionId: string): Promise<void> {
  if (busy) return;

  current = sessionId;
  dom.session.textContent = sessionId;
  dom.older.hidden = true;

  try {
    await loadHistory(sessionId, null);
  } catch (error) {
    dom.messages.replaceChildren(failureOf(error));
    return;
  }
  scrollToEnd();
}

/**
 * 拉一页历史画上去。
 *
 * `cursor` 为空 = 换会话或首次打开，整屏换掉；给了 = 往上翻，接在最前面。
 * 这正是 `session/messages` 的用法：协议给的是**最新的在最前**（方便一页页
 * 往更早走），而屏幕上要正的（自下而上），所以在这儿翻一次。
 */
async function loadHistory(sessionId: string, cursor: string | null): Promise<void> {
  const page = await fetchMessages(sessionId, cursor);
  const nodes = [...page.data].reverse().map(messageRow);

  if (cursor === null) {
    dom.messages.replaceChildren(
      ...(nodes.length === 0 ? [muted("这个会话还没说过话。说第一句就有了。")] : nodes),
    );
  } else {
    // 接在最上面，并且**保持原来的滚动位置** —— 否则翻历史会把眼前那段弹走。
    const fromBottom = dom.messages.scrollHeight - dom.messages.scrollTop;
    dom.messages.prepend(...nodes);
    dom.messages.scrollTop = dom.messages.scrollHeight - fromBottom;
  }

  olderCursor = page.nextCursor;
  dom.older.hidden = olderCursor === null;
}

/** 往前翻一页：更早的话。 */
async function older(): Promise<void> {
  if (busy || olderCursor === null) return;

  dom.older.disabled = true;
  try {
    await loadHistory(current, olderCursor);
  } catch (error) {
    // 失败了就把这句话留在最上面，别让人以为「点了一下什么都没发生」。
    dom.messages.prepend(failureOf(error));
  } finally {
    dom.older.disabled = false;
  }
}

function setBusy(value: boolean): void {
  busy = value;
  dom.send.disabled = value;
  dom.composer.classList.toggle("busy", value);
}

function scrollToEnd(): void {
  dom.messages.scrollTop = dom.messages.scrollHeight;
}

function must<T extends HTMLElement = HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  // 找不到就当场炸掉。静默什么都不做，比报错难查得多 ——
  // 而且这只可能是 index.html 和这里对不上了。
  if (node === null) throw new Error(`页面里没有 #${id}`);
  return node as T;
}

void boot();
