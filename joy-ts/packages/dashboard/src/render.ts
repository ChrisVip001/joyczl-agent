// 把数据画成 DOM 的那些函数。
//
// 纯函数居多：给它数据，还你一个节点。这样 main.ts 只管「什么时候重画」，
// 不管「长什么样」—— 页面变丑的时候只需要看这一个文件。

import type {
  DashboardData,
  Episode,
  Fact,
  Message,
  SessionSummary,
  TokenUsage,
  TurnCompletedNotification,
  TurnMeta,
} from "./protocol";

type Attrs = Record<string, string | number | boolean | undefined>;

/**
 * 建一个元素。
 *
 * 文本一律走 `textContent`：模型说的话、记忆里的内容、文件路径都是外部输入，
 * 没有理由让它们有机会变成 HTML。`innerHTML` 在这个页面里没有用武之地。
 */
export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs = {},
  ...children: (Node | string)[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value === undefined || value === false) continue;
    if (key === "class") node.className = String(value);
    else if (key === "text") node.textContent = String(value);
    else node.setAttribute(key, String(value));
  }
  for (const child of children) node.append(child);
  return node;
}

/** 概览面板：现在是什么状态。`onPick` 是点了某个会话之后要做的事（切过去）。 */
export function renderOverview(
  data: DashboardData,
  session: string,
  onPick: (id: string) => void,
): DocumentFragment {
  const { config } = data;
  const fragment = document.createDocumentFragment();

  fragment.append(
    el(
      "div",
      { class: "headline" },
      el("div", { class: "provider", text: config.provider }),
      el("div", { class: "model", text: config.model }),
    ),
    el("div", { class: "path", text: config.home, title: config.home }),
    numbers([
      ["会话", data.sessions.length],
      ["事实", data.facts.length],
      ["情景", data.episodes.length],
    ]),
  );

  fragment.append(
    section(
      "怎么想的",
      el(
        "dl",
        { class: "rows" },
        ...row("小模型", config.smallModel),
        ...row("工作记忆", `最近 ${config.historyTurns} 轮`),
        ...row("提炼频率", `每 ${config.consolidateEvery} 轮`),
        ...row("检索条数", String(config.retrievalTopK)),
        ...row("最多迭代", String(config.maxIterations)),
        ...row(
          "实验",
          [
            config.experimental ? "experimental" : null,
            config.graphWorkflows ? "graph" : null,
            config.appleCalendar ? "apple" : null,
            config.googleCalendar ? "google" : null,
          ]
            .filter((flag) => flag !== null)
            .join(" · ") || "关",
        ),
      ),
    ),
  );

  fragment.append(
    section(
      `最近会话（当前：${session}）`,
      data.sessions.length === 0
        ? muted("还没有会话。说第一句话就有了。")
        : el(
            "ul",
            { class: "list" },
            ...data.sessions.map((item) => sessionRow(item, session, onPick)),
          ),
    ),
  );

  return fragment;
}

/** 记忆面板：它记得什么。 */
export function renderMemory(data: DashboardData): DocumentFragment {
  const fragment = document.createDocumentFragment();

  fragment.append(
    section(
      `事实 ${data.facts.length}`,
      data.facts.length === 0
        ? muted("还没有事实。跟它说「记住…」，或者等对话被提炼。")
        : el("ul", { class: "list" }, ...data.facts.map(factRow)),
    ),
  );

  fragment.append(
    section(
      `情景 ${data.episodes.length}`,
      data.episodes.length === 0
        ? muted("还没有情景。聊过的每一天会在这里留一行摘要。")
        : el("ul", { class: "list" }, ...data.episodes.map(episodeRow)),
    ),
  );

  return fragment;
}

/** 一条通知说过的话（gate / 工具 / 图），做成一个小胶囊。 */
export function chip(text: string, kind: "gate" | "tool" | "graph" | "note" = "note"): HTMLElement {
  return el("span", { class: `chip ${kind}`, text });
}

/** 错误：红色的那一条。协议里的码也留着 —— 查日志的时候有用。 */
export function failure(error: { code: number; message: string }): HTMLElement {
  return el(
    "div",
    { class: "failure" },
    el("span", { class: "code", text: String(error.code) }),
    el("span", { text: error.message }),
  );
}

export function failureOf(error: unknown): HTMLElement {
  if (error instanceof Error) {
    const code = (error as { code?: unknown }).code;
    return failure({
      code: typeof code === "number" ? code : 0,
      message: error.message,
    });
  }
  return failure({ code: 0, message: String(error) });
}

/** 一轮结束时的那行小字：用哪个模型、花多久、多少 token。 */
export function turnFooter(turn: TurnCompletedNotification): string {
  const parts = [
    turn.meta.model,
    `${turn.meta.iterations} 轮`,
    formatMs(turn.meta.latencyMs),
  ];
  if (turn.usage !== null && turn.usage !== undefined) parts.push(formatUsage(turn.usage));
  if (turn.meta.tools.length > 0) parts.push(`${turn.meta.tools.length} 次工具`);
  return parts.join(" · ");
}

/**
 * 历史里的一条消息。
 *
 * 刻意画成跟**实时那一路**同一个样子（同样的 class、同样的布局）：刷新之后
 * 那句话不该看起来像换了个客户端 —— 它们本来就是同一场对话的两个来源。
 */
export function messageRow(message: Message): HTMLElement {
  const bubble = el("div", { class: "bubble", text: message.content });

  if (message.role === "user") {
    return el("div", { class: "msg me" }, bubble);
  }

  // 遥测是唯一能还原「当时怎么想的」的东西，所以它值得被画出来 ——
  // 但它是**一轮的摘要**，不是逐条回放：实时视图里每次工具调用会飘两个胶囊
  // （开始、结束），这里一个工具只留一个。
  const meta = message.meta;
  const trace = el(
    "div",
    { class: "trace" },
    ...(meta === null || meta === undefined ? [] : metaChips(meta)),
  );

  return el(
    "div",
    { class: "msg joy" },
    trace,
    bubble,
    el("div", { class: "foot", text: historyFooter(message) }),
  );
}

/** 落库的遥测 → 跟实时视图长得一样的胶囊。 */
function metaChips(meta: TurnMeta): HTMLElement[] {
  const chips: HTMLElement[] = [];

  const gate = meta.gate;
  if (gate !== null && gate !== undefined) {
    chips.push(
      chip(
        gate.decision === "retrieve"
          ? `检索「${gate.query ?? ""}」`
          : `跳过记忆（${gate.reason}）`,
        "gate",
      ),
    );
  }

  for (const tool of meta.tools) {
    chips.push(chip(`工具 · ${tool.tool} ${tool.status === "ok" ? "好了" : "出错"}`, "tool"));
  }

  return chips;
}

/**
 * 历史里那行小字。
 *
 * **没有 token 数**：usage 只走过 `turnCompleted`，从来没进库 —— 与其在这儿
 * 猜一个数字，不如只写库里真有的（模型、迭代、耗时、什么时候说的）。
 */
function historyFooter(message: Message): string {
  const parts = [formatWhen(message.at)];
  const meta = message.meta;
  if (meta !== null && meta !== undefined) {
    parts.unshift(meta.model, `${meta.iterations} 轮`, formatMs(meta.latencyMs));
  }
  return parts.join(" · ");
}

// --- 小零件 ---------------------------------------------------------------

function section(title: string, body: Node): HTMLElement {
  return el("div", { class: "block" }, el("h3", { text: title }), body);
}

/** 「这儿什么都没有」的那句话。 */
export function muted(text: string): HTMLElement {
  return el("p", { class: "muted", text });
}

function numbers(pairs: [string, number][]): HTMLElement {
  return el(
    "div",
    { class: "numbers" },
    ...pairs.map(([label, value]) =>
      el(
        "div",
        { class: "number" },
        el("b", { text: String(value) }),
        el("span", { text: label }),
      ),
    ),
  );
}

function row(label: string, value: string): [HTMLElement, HTMLElement] {
  return [el("dt", { text: label }), el("dd", { text: value, title: value })];
}

/**
 * 一行会话。**可以点** —— 点了就切过去。
 *
 * 里层是 `<button>` 而不是往 `<li>` 上挂 click：一个能点、能有 hover 的东西
 * 本来就该是按钮，键盘和读屏也才认它（`<div>` + click 两样都没有）。
 * 内容是 `<span>` 而不是 `<div>`：按钮里只允许放行内元素。
 */
function sessionRow(
  item: SessionSummary,
  current: string,
  onPick: (id: string) => void,
): HTMLElement {
  const classes = item.id === current ? "item pick current" : "item pick";
  const button = el(
    "button",
    { class: classes, type: "button", title: `切到会话「${item.id}」` },
    el("span", { class: "title", text: item.title }),
    el("span", {
      class: "sub",
      text: `${item.messages} 条 · ${formatWhen(item.lastAt)}`,
    }),
  );
  button.addEventListener("click", () => onPick(item.id));
  return el("li", {}, button);
}

function factRow(fact: Fact): HTMLElement {
  return el(
    "li",
    { class: "item" },
    el(
      "div",
      { class: "title" },
      el("span", { class: "subject", text: fact.subject }),
      el("span", { class: `tag ${fact.source}`, text: fact.source === "user" ? "你说" : "提炼" }),
    ),
    el("div", { class: "body", text: fact.content }),
  );
}

function episodeRow(episode: Episode): HTMLElement {
  return el(
    "li",
    { class: "item" },
    el("div", { class: "when", text: episode.happenedAt }),
    el("div", { class: "body", text: episode.summary }),
  );
}

// --- 格式化 ---------------------------------------------------------------

/** ISO 时间 → 「刚刚 / 12 分钟前 / 昨天 09:31 / 8月3日」。 */
export function formatWhen(iso: string | null | undefined): string {
  if (iso === null || iso === undefined || iso === "") return "—";
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return iso;

  const minutes = (Date.now() - at.getTime()) / 60_000;
  if (minutes < 1) return "刚刚";
  if (minutes < 60) return `${Math.floor(minutes)} 分钟前`;
  if (minutes < 60 * 24) return clock(at);
  return `${at.getMonth() + 1} 月 ${at.getDate()} 日`;
}

/** 页头那枚时间戳：数据是什么时候的。 */
export function formatStamp(iso: string): string {
  const at = new Date(iso);
  return Number.isNaN(at.getTime()) ? iso : `${clock(at)} 更新`;
}

export function formatMs(ms: number): string {
  return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;
}

function formatUsage(usage: TokenUsage): string {
  return `${usage.inputTokens}→${usage.outputTokens} token`;
}

function clock(at: Date): string {
  return at.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false });
}
