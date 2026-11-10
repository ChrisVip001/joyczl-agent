// The functions that paint data into DOM.
//
// Mostly pure functions: give data, get back a node. main.ts only manages "when to repaint",
// not "what it looks like" — when the page gets ugly, look at this one file.

import type {
  AgentTaskView,
  ApprovalLogEntry,
  DashboardData,
  Episode,
  Fact,
  GoalSetResponse,
  Message,
  ScheduleStateRow,
  TodoListView,
  SessionSummary,
  TokenUsage,
  ToolStartedNotification,
  TurnCompletedNotification,
  TurnMeta,
  UsageSummaryResponse,
  UsageByModel,
} from "./protocol";

type Attrs = Record<string, string | number | boolean | undefined | (() => void)>;

/**
 * Build an element.
 *
 * Text always goes through `textContent`: what the model says, memory contents, file paths are
 * all external input, with no reason to ever become HTML. `innerHTML` has no use in this page.
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
    // `on<event>` takes a function: listeners are added, not stringified into attributes.
    else if (key.startsWith("on") && typeof value === "function") {
      node.addEventListener(key.slice(2), value as EventListener);
    } else node.setAttribute(key, String(value));
  }
  for (const child of children) node.append(child);
  return node;
}

/** Overview panel: the current state. `onPick` is what happens after a session is clicked (switch). */
export function renderOverview(
  data: DashboardData,
  session: string,
  onPick: (id: string) => void,
  onTask: (task: AgentTaskView, action: "suspend" | "resume" | "cancel") => void,
  onInbox: () => void,
): DocumentFragment {
  const { config } = data;
  const fragment = document.createDocumentFragment();

  // The task card leads the overview when there is anything on it: what the loop is
  // chasing, what the model ticked off — the two answers to "what is it doing now".
  if (data.goal !== undefined || data.todos !== undefined || data.task != null) {
    const emptyTodos: TodoListView = { revision: 0n, items: [] };
    fragment.append(
      renderTaskCard(
        data.goal ?? { active: false, condition: null, roundsUsed: 0, maxRounds: 0 },
        data.todos ?? emptyTodos,
        data.task,
        onTask,
      ),
    );
  }

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

  if (data.inbox.items.length > 0 || data.inbox.unseen > 0) {
    fragment.append(renderInbox(data, onInbox));
  }

  if (data.connectors.length > 0 || data.connectorsError != null) {
    fragment.append(renderConnectors(data));
  }

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

/**
 * The inbox: what came in, and the single button that turns it into work.
 *
 * Read-only otherwise — polling never marks mail as read, and nothing here replies. The
 * list is capped by the app-server (20); this shows the newest five so the panel stays a
 * glance, not a mail client.
 */
function renderInbox(data: DashboardData, onInbox: () => void): HTMLElement {
  const block = el(
    "div",
    { class: "block inbox" },
    el("h3", {
      text: data.inbox.unseen > 0 ? `收件箱 · ${data.inbox.unseen} 封等你处理` : "收件箱",
    }),
  );
  for (const item of data.inbox.items.slice(0, 5)) {
    block.append(
      el(
        "div",
        { class: `inbox-row${item.seen ? "" : " unread"}` },
        el("div", { class: "inbox-meta", text: `${item.fromAddr} · ${item.receivedAt}` }),
        el("div", { class: "inbox-subject", text: item.subject }),
        el("div", { class: "inbox-snippet", text: item.snippet }),
      ),
    );
  }
  if (data.inbox.unseen > 0) {
    block.append(
      el("button", {
        class: "task-act",
        type: "button",
        text: "处理这些邮件",
        onclick: onInbox,
      }),
    );
  }
  block.append(
    el("p", {
      class: "connector-note",
      text: "只看不标已读；处理是把它们交给一个回合，不会替你回信。",
    }),
  );
  return block;
}

/// Connectors: one quiet row each — name, whether it can actually send, allowlist width.
/// Read-only on purpose: the switch lives in connectors.json, a file only the human edits.
function renderConnectors(data: DashboardData): HTMLElement {
  const names: Record<string, string> = { email: "邮件", feishu: "飞书" };
  const block = el("div", { class: "block connectors" }, el("h3", { text: "连接器" }));
  if (data.connectorsError !== null) {
    block.append(el("div", { class: "connector-broken", text: data.connectorsError }));
  }
  for (const connector of data.connectors) {
    block.append(
      el(
        "div",
        { class: "connector-row" },
        el("span", {
          class: "connector-name",
          text: names[connector.id] ?? connector.id,
        }),
        el("span", {
          class: `tag connector-mode ${connector.live ? "live" : "draft"}`,
          text: connector.live ? "可发送" : "草稿",
        }),
        el("span", {
          class: "connector-allow",
          text:
            connector.allowRecipients === 0
              ? "白名单空 —— 谁都不能收"
              : `白名单 ${connector.allowRecipients} 个`,
        }),
      ),
    );
  }
  block.append(
    el("p", {
      class: "connector-note",
      text: "开关在 connectors.json 里 —— 只有人能改；实际发送每次都会问。",
    }),
  );
  return block;
}

/**
 * The task card: the goal loop's state plus the model's own todo sheet, as the human
 * sees them. Both come from `DashboardData` (goal/todos, filled only when the page
 * names a session) — this is where "what is the agent actually working on" lives.
 */
export function renderTaskCard(
  goal: GoalSetResponse,
  todos: TodoListView,
  task: AgentTaskView | undefined,
  onTask: (task: AgentTaskView, action: "suspend" | "resume" | "cancel") => void,
): DocumentFragment {
  const fragment = document.createDocumentFragment();
  const block = el("div", { class: "block taskcard" });

  // The task leads: a goal that survives restarts is a noun with a status, not a condition.
  // Pause/continue are **human switches** — the card is the only place they exist.
  const statusText: Record<string, string> = {
    pending: "等待开始",
    running: "进行中",
    suspended: "已暂停",
    done: "已完成",
    failed: "未达成",
    cancelled: "已取消",
  };
  if (task != null) {
    const live = task.status === "running" || task.status === "pending";
    block.append(
      el("h3", { text: "任务" }),
      el(
        "div",
        { class: "task-goal" },
        el("span", { class: "task-goal-text", text: task.title }),
        el("span", {
          class: `tag task-state st-${task.status}`,
          text: `${statusText[task.status] ?? task.status}${
            task.goalCondition !== null ? ` · 第 ${task.roundsUsed}/${task.maxRounds} 轮` : ""
          }`,
        }),
      ),
      task.suspendReason !== null
        ? el("div", { class: "task-note", text: task.suspendReason })
        : document.createDocumentFragment(),
      el(
        "div",
        { class: "task-actions" },
        ...(live
          ? [
              el("button", {
                class: "task-act",
                type: "button",
                text: "暂停",
                onclick: () => onTask(task, "suspend"),
              }),
            ]
          : []),
        ...(task.status === "suspended"
          ? [
              el("button", {
                class: "task-act task-act-go",
                type: "button",
                text: "继续",
                onclick: () => onTask(task, "resume"),
              }),
            ]
          : []),
        ...(live || task.status === "suspended"
          ? [
              el("button", {
                class: "task-act task-act-stop",
                type: "button",
                text: "取消",
                onclick: () => onTask(task, "cancel"),
              }),
            ]
          : []),
      ),
    );
  }

  // A goal set on an existing task has its own text; when it is just the title repeated,
  // the task block above already says it.
  const duplicated = task != null && task.goalCondition === task.title;
  if (goal.active && goal.condition !== null && !duplicated) {
    block.append(
      el("h3", { text: "目标" }),
      el(
        "div",
        { class: "task-goal" },
        el("span", { class: "task-goal-text", text: goal.condition }),
        // The task block above already carries the round account; don't say it twice.
        task == null
          ? el("span", { class: "tag", text: `第 ${goal.roundsUsed}/${goal.maxRounds} 轮` })
          : document.createDocumentFragment(),
      ),
    );
  }

  if (todos.items.length > 0) {
    block.append(
      el("h3", { text: "待办" }),
      el(
        "ul",
        { class: "task-todos" },
        ...todos.items.map((item) =>
          el(
            "li",
            { class: `task-todo st-${item.status}` },
            el("span", { class: "task-glyph", "aria-hidden": "true" }),
            el("span", { class: "task-text", text: item.content }),
          ),
        ),
      ),
    );
  }

  if (block.children.length > 0) fragment.append(block);
  return fragment;
}

/**
 * The sidebar's session list — same rows as the overview's, one source of truth
 * (`sessionRow`), two surfaces. The sidebar is where switching actually happens;
 * the overview keeps the fuller picture.
 */
export function sessionList(
  data: DashboardData,
  current: string,
  onPick: (id: string) => void,
): DocumentFragment {
  const fragment = document.createDocumentFragment();
  const sessions = [...data.sessions];
  // A brand-new session has no messages yet, so /api/data cannot list it. Keep it on screen
  // and selected until the first turn gives it a real title — otherwise "new chat" looks broken.
  if (!sessions.some((item) => item.id === current)) {
    const fresh: SessionSummary = {
      id: current,
      title: "新会话",
      messages: 0,
      startedAt: null,
      lastAt: null,
    };
    sessions.unshift(fresh);
  }
  if (sessions.length === 0) {
    fragment.append(muted("还没有会话"));
    return fragment;
  }
  fragment.append(
    el("ul", { class: "list" }, ...sessions.map((item) => sessionRow(item, current, onPick))),
  );
  return fragment;
}

/** Memory panel: what it remembers. */
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

/** What a notification said (gate / tool / graph), as a small capsule. */
export function chip(text: string, kind: "gate" | "tool" | "graph" | "note" = "note"): HTMLElement {
  return el("span", { class: `chip ${kind}`, text });
}

/**
 * Error: a quiet red card, not an alarm. The protocol code stays as a tag —
 * useful when checking logs, meaningless to a person.
 */
export function failure(error: { code: number; message: string }): HTMLElement {
  return el(
    "div",
    { class: "failure" },
    el(
      "div",
      { class: "failure-head" },
      el("span", { class: "failure-dot", "aria-hidden": "true" }),
      el("span", { class: "failure-title", text: "出错了" }),
      el("span", { class: "code", text: String(error.code) }),
    ),
    el("div", { class: "failure-body", text: error.message }),
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

/** The small line when a turn ends: which model, how long, how many tokens. */
export function turnFooter(turn: TurnCompletedNotification): string {
  const parts = [
    turn.meta.model,
    `${turn.meta.iterations} 轮`,
    formatMs(turn.meta.latencyMs),
  ];
  if (turn.usage !== null && turn.usage !== undefined) parts.push(formatUsage(turn.usage));
  if (turn.meta.tools.length > 0) parts.push(`${turn.meta.tools.length} 次工具`);
  if (turn.meta.interrupted === true) parts.push("已被你停止");
  // Route reason only speaks when a choice/fallback actually happened (empty in fixed/active).
  if (turn.meta.routeReason) parts.push(`路由 ${turn.meta.routeReason}`);
  return parts.join(" · ");
}

/**
 * The empty-session welcome: a quiet hero instead of an orphan sentence.
 * Rendered only when a session has no history; the first message replaces it.
 */
export function welcome(): HTMLElement {
  return el(
    "div",
    { class: "welcome" },
    el("p", { class: "welcome-title", text: "你好，我是 Joy" }),
    el("p", {
      class: "welcome-sub",
      text: "把要做的事说清楚就行 —— 长任务会一步步来，需要你拍板时会停下来问。",
    }),
  );
}

/**
 * The attachment strip under a sent message: thumbnails for images, name chips for the rest.
 * `url` is an object URL minted by main (it owns the lifecycle); without one, name only.
 */
export function attachmentStrip(files: { name: string; url?: string }[]): HTMLElement {
  const strip = el("div", { class: "attach-strip" });
  for (const file of files) {
    if (file.url !== undefined) {
      const img = el("img", {
        class: "attach-thumb",
        src: file.url,
        alt: file.name,
        title: file.name,
      });
      // A failed object URL must not leave a broken-image glyph behind.
      img.onerror = () => {
        img.replaceWith(el("span", { class: "tag", text: file.name }));
      };
      strip.append(img);
    } else {
      strip.append(el("span", { class: "tag", text: file.name, title: file.name }));
    }
  }
  return strip;
}

/**
 * One message from history.
 *
 * Deliberately painted identical to the **live stream** (same classes, same layout): after a
 * refresh a message shouldn't look like a different client — they're two sources of one talk.
 */
export function messageRow(message: Message): HTMLElement {
  const bubble = el("div", { class: "bubble", text: message.content });

  if (message.role === "user") {
    return el("div", { class: "msg me" }, bubble);
  }

  // Telemetry is the only thing that can reconstruct "what it was thinking then", so it's worth
  // painting — but it's a **turn summary**, not a per-item replay: in the live view each tool
  // call floats two capsules (start, end); here one tool keeps one.
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

/** Persisted telemetry → capsules identical to the live view. */
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
 * The small line in history.
 *
 * **No token counts**: usage only passed through `turnCompleted` and never entered the store —
 * rather than guess a number, write only what the store truly has (model, iterations, timing).
 */
function historyFooter(message: Message): string {
  const parts = [formatWhen(message.at)];
  const meta = message.meta;
  if (meta !== null && meta !== undefined) {
    parts.unshift(meta.model, `${meta.iterations} 轮`, formatMs(meta.latencyMs));
  }
  return parts.join(" · ");
}

// --- Small parts ---------------------------------------------------------------

function section(title: string, body: Node): HTMLElement {
  return el("div", { class: "block" }, el("h3", { text: title }), body);
}

/** The "there's nothing here" line. */
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
 * One session row. **Clickable** — click to switch to it.
 *
 * The inner element is a `<button>` rather than a click on `<li>`: something clickable and
 * hoverable should be a button; only then keyboard and screen readers recognize it (`<div>` +
 * click has neither). The content is a `<span>` not a `<div>`: buttons allow inline elements.
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

// --- Formatting ---------------------------------------------------------------

/** ISO time → "just now / 12 minutes ago / yesterday 09:31 / Aug 3". */
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

/** The header timestamp: when the data is from. */
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

// --- New panels (usage / approvals / schedules) and tool cards ---------------------------------

/** Usage panel: totals + per-model aggregation. Bookkeeping only — a ledger, not a quota. */
export function renderUsage(data: UsageSummaryResponse): DocumentFragment {
  const fragment = document.createDocumentFragment();
  // In generated types i64 is bigint — the display layer always falls back to number via Number().
  fragment.append(
    numbers([
      ["轮次", Number(data.turns)],
      ["输入 token", Number(data.inputTokens)],
      ["输出 token", Number(data.outputTokens)],
    ]),
  );

  fragment.append(
    section(
      "按模型",
      data.byModel.length === 0
        ? muted("还没有用量。聊一轮就有了。")
        : el(
            "ul",
            { class: "list" },
            // Denominator for relative bar widths: the largest input across all models.
            ...data.byModel.map((row) => usageRow(row, maxInput(data.byModel))),
          ),
    ),
  );
  return fragment;
}

/** Denominator for relative bar widths: the largest input across models (at least 1, no div0). */
function maxInput(rows: UsageByModel[]): number {
  return Math.max(...rows.map((row) => Number(row.inputTokens)), 1);
}

function usageRow(row: UsageByModel, maxInput: number): HTMLElement {
  // Bar length follows "that model's input vs the largest" — no total scale, only relative size,
  // which is what the eye wants: who's eating tokens is visible at a glance.
  return el(
    "li",
    { class: "item" },
    el(
      "div",
      { class: "title" },
      el("span", { class: "subject", text: `${row.provider}/${row.model}` }),
      el("span", { class: "tag", text: `${Number(row.turns)} 轮` }),
    ),
    el(
      "div",
      { class: "meter", role: "img", "aria-label": `${row.inputTokens} 输入 token` },
      el("div", {
        class: "meter-fill",
        style: `width:${Math.min(100, Math.round((Number(row.inputTokens) / maxInput) * 100))}%`,
      }),
    ),
    el("div", {
      class: "sub",
      text: `输入 ${Number(row.inputTokens).toLocaleString()} · 输出 ${Number(row.outputTokens).toLocaleString()}`,
    }),
  );
}

const OUTCOME_LABEL: Record<string, string> = {
  approved: "已允许",
  denied: "已拒绝",
  timeout: "没人答",
  voided: "已作废",
};

const OUTCOME_KIND: Record<string, string> = {
  approved: "good",
  denied: "bad",
  timeout: "",
  voided: "",
};

/** Approval log panel: who asked, what happened. For a hub admin answering "who approved rm -rf". */
export function renderApprovals(entries: ApprovalLogEntry[]): DocumentFragment {
  const fragment = document.createDocumentFragment();
  fragment.append(
    section(
      "最近 100 条",
      entries.length === 0
        ? muted("还没有「问了才跑」的记录。开启审批后，每次询问都会留一行。")
        : el("ul", { class: "list" }, ...entries.map(approvalRow)),
    ),
  );
  return fragment;
}

function approvalRow(entry: ApprovalLogEntry): HTMLElement {
  const kind = OUTCOME_KIND[entry.outcome] ?? "";
  return el(
    "li",
    { class: "item" },
    el(
      "div",
      { class: "title" },
      el("span", { class: "subject", text: entry.tool }),
      el("span", { class: `tag ${kind}`, text: OUTCOME_LABEL[entry.outcome] ?? entry.outcome }),
    ),
    el("code", { class: "cmd", text: entry.argsPreview }),
    el("div", { class: "sub", text: `${formatWhen(entry.decidedAt)} · ${entry.reason || "—"}` }),
  );
}

/** Schedules panel: each job's policy, last trigger, and any interrupted unsettled claim. */
export function renderSchedules(jobs: ScheduleStateRow[]): DocumentFragment {
  const fragment = document.createDocumentFragment();
  fragment.append(
    section(
      "登记的任务",
      jobs.length === 0
        ? muted("没有定时任务。给 SKILL.md 加 schedule: 或在 schedules.json 里写 jobs。")
        : el("ul", { class: "list" }, ...jobs.map(scheduleRow)),
    ),
  );
  return fragment;
}

function scheduleRow(job: ScheduleStateRow): HTMLElement {
  const children: (Node | string)[] = [
    el(
      "div",
      { class: "title" },
      el("span", { class: "subject", text: job.name }),
      el("span", { class: "tag", text: job.delivery }),
      el("span", { class: "tag", text: job.catchUp ? "补跑" : "放弃" }),
    ),
    el("div", {
      class: "sub",
      text: `上次触发：${job.lastFiredMinute ?? "还没跑过"}`,
    }),
  ];
  // An unsettled claim = the last run was interrupted — the line this panel must highlight.
  if (job.claimedMinute !== null && job.claimedMinute !== undefined) {
    children.push(
      el("div", {
        class: "warn",
        text: `中断于 ${job.claimedMinute}，等调度器按「${job.catchUp ? "补跑" : "放弃"}」接管`,
      }),
    );
  }
  return el("li", { class: "item" }, ...children);
}

// --- Tool card registry ----------------------
//
// Each tool extracts a **plain-language summary** from `args` instead of spitting out JSON.
// The registry is just an object literal — a new tool adds a line, no catch-all abstraction.

type ToolArgs = Record<string, unknown>;

function argString(args: ToolArgs, ...keys: string[]): string {
  for (const key of keys) {
    const value = args[key];
    if (typeof value === "string" && value !== "") return value;
  }
  return "";
}

/** Tool name → a one-line summary of "what this step is doing". */
const TOOL_SUMMARY: Record<string, (args: ToolArgs) => string> = {
  run_command: (args) => argString(args, "command"),
  read_file: (args) => argString(args, "path"),
  write_file: (args) => argString(args, "path"),
  edit_file: (args) => argString(args, "path"),
  list_dir: (args) => argString(args, "path") || ".",
  search_file: (args) => argString(args, "pattern"),
  search_content: (args) => argString(args, "pattern"),
  fetch_url: (args) => argString(args, "url"),
  web_search: (args) => argString(args, "query"),
  delegate_task: (args) => argString(args, "task", "description"),
  remember: (args) => argString(args, "content"),
  todo_write: (args) => {
    const todos = args["todos"];
    return Array.isArray(todos) ? `${todos.length} 项` : "看一眼";
  },
};

/** A one-line plain-language summary of "what this step is doing" (empty = show name only). */
export function toolSummary(notification: ToolStartedNotification): string {
  const args =
    typeof notification.args === "object" && notification.args !== null
      ? (notification.args as ToolArgs)
      : {};
  const summarize = TOOL_SUMMARY[notification.tool];
  return summarize ? summarize(args) : "";
}

/** The stopwatch text in a card. `start` is the moment this card was created. */
export function elapsedSince(start: number): string {
  return formatMs(Date.now() - start);
}
