// The functions that paint data into DOM.
//
// Mostly pure functions: give data, get back a node. main.ts only manages "when to repaint",
// not "what it looks like" — when the page gets ugly, look at this one file.

import { renderMarkdown } from "./markdown";
import type {
  AgentTaskView,
  ArtifactView,
  AcceptanceView,
  BudgetView,
  CheckView,
  DashboardData,
  Episode,
  Fact,
  GoalSetResponse,
  Message,
  TodoListView,
  SessionSummary,
  TokenUsage,
  ToolStartedNotification,
  TurnCompletedNotification,
  TurnMeta,
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
        data.taskArtifacts ?? [],
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
  artifacts: ArtifactView[],
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
    // Not a failure: the machine measured something it must not overrule, so a person looks.
    needs_attention: "需要你确认",
    done: "已完成",
    failed: "未达成",
    cancelled: "已取消",
  };
  if (task != null) {
    const live = task.status === "running" || task.status === "pending";
    // "Needs you" is resumable on purpose: continue is exactly what this state is for.
    const resumable = task.status === "suspended" || task.status === "needs_attention";
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
      // ---- 验收清单：每条条件一行；通过给勾，未通过给叉和实测结果。
      // 没有验收条件时整块不出现 —— 空的"验收"标题只是噪音。
      ...acceptanceBlock(task.acceptance, task.checks),
      // ---- 交付物：真的存在的文件，不是模型的总结。
      ...artifactBlock(artifacts),
      // ---- 预算：只有设了上限的维度才画条（没上限的只显示已用）。
      ...budgetBlock(task.budget, task.spent),
      // ---- 脚注：执行几次、上次停在哪。
      taskFooter(task),
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
        ...(resumable
          ? [
              el("button", {
                class: "task-act task-act-go",
                type: "button",
                text: "继续",
                onclick: () => onTask(task, "resume"),
              }),
            ]
          : []),
        ...(live || resumable
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

/** At most this many condition rows before folding into "还有 N 条". */
const CHECKS_SHOWN = 6;
/** At most this many deliverables before folding. */
const ARTIFACTS_SHOWN = 5;

/**
 * The acceptance checklist: **the reason** a task is not done, in the same words the checker
 * used. `checks` is index-aligned with `acceptance`; a condition with no verdict yet says so
 * instead of pretending to pass.
 */
function acceptanceBlock(
  acceptance: AcceptanceView[],
  checks: CheckView[],
): Node[] {
  if (acceptance.length === 0) return [];
  const rows = acceptance.slice(0, CHECKS_SHOWN).map((condition, index) => {
    const check = checks[index];
    const passed = check?.passed === true;
    return el(
      "li",
      { class: `task-check ${passed ? "ok" : "bad"}` },
      el("span", { class: "task-glyph", "aria-hidden": "true" }),
      el("span", {
        class: "task-check-label",
        text: condition.label ?? check?.label ?? condition.kind,
      }),
      el("span", {
        class: "task-check-detail",
        // Passed: the evidence ("report.md 12.4 KB"). Failed: the measurement.
        text: check?.detail ?? "尚未检查",
      }),
    );
  });
  const rest = acceptance.length - rows.length;
  return [
    el("h3", { text: "验收" }),
    el("ul", { class: "task-checks" }, ...rows),
    rest > 0 ? el("div", { class: "task-more", text: `还有 ${rest} 条` }) : document.createDocumentFragment(),
  ];
}

/** The deliverables: files that exist, with their real sizes. */
function artifactBlock(artifacts: ArtifactView[]): Node[] {
  if (artifacts.length === 0) return [];
  const rows = artifacts.slice(0, ARTIFACTS_SHOWN).map((artifact) =>
    el(
      "li",
      { class: "task-artifact", title: artifact.path },
      el("span", { class: "task-artifact-name", text: baseName(artifact.path) }),
      el("span", { class: "task-artifact-size", text: formatBytes(artifact.bytes) }),
    ),
  );
  const rest = artifacts.length - rows.length;
  return [
    el("h3", { text: "交付物" }),
    el("ul", { class: "task-artifacts" }, ...rows),
    rest > 0 ? el("div", { class: "task-more", text: `还有 ${rest} 个` }) : document.createDocumentFragment(),
  ];
}

/**
 * Three usage bars (time / tokens / tool calls). A dimension with no cap still shows what was
 * spent — the numbers are the honest part; the bar only exists where there is a limit to hit.
 */
function budgetBlock(
  budget: BudgetView | null,
  spent: { wallMs: number; tokens: number; tools: number },
): Node[] {
  if (budget === null) return [];
  // One unit per row: both numbers must read in the same scale, or "45.2s / 10 分 0 秒"
  // happens — two ways of saying time side by side looks like a bug.
  const clock = (budget.wallMs ?? spent.wallMs) >= 60_000;
  const time = (value: number) => formatDuration(value, clock);
  const rows = [
    budgetRow("时长", time(spent.wallMs), spent.wallMs, budget.wallMs, time),
    budgetRow("Token", String(spent.tokens), spent.tokens, budget.tokens, String),
    budgetRow("工具调用", String(spent.tools), spent.tools, budget.tools, String),
  ].filter((row): row is HTMLDivElement => row !== null);
  if (rows.length === 0) return [];
  return [el("h3", { text: "预算" }), el("div", { class: "task-budget" }, ...rows)];
}

function budgetRow(
  name: string,
  shown: string,
  spent: number,
  cap: number | null,
  render: (value: number) => string,
): HTMLDivElement | null {
  if (cap === null && spent === 0) return null;
  const ratio = cap !== null && cap > 0 ? spent / cap : null;
  const level = ratio === null ? "" : ratio >= 1 ? " is-over" : ratio >= 0.8 ? " is-warn" : "";
  return el(
    "div",
    { class: "task-budget-row" },
    el(
      "div",
      { class: "task-budget-line" },
      el("span", { class: "task-budget-name", text: name }),
      el("span", {
        class: "task-budget-num",
        text: cap === null ? shown : `${shown} / ${render(cap)}`,
      }),
    ),
    ratio === null
      ? document.createDocumentFragment()
      : el(
          "div",
          { class: `meter${level}`, role: "img", "aria-label": `${name} ${shown} / ${render(cap as number)}` },
          el("div", {
            class: "meter-fill",
            style: `width:${Math.min(100, Math.round(ratio * 100))}%`,
          }),
        ),
  );
}

/** "执行 3 次 · 上次停在「验收未通过：report.md 不存在」· 未设预算" */
function taskFooter(task: AgentTaskView): Node {
  const parts: string[] = [];
  parts.push(task.runCount > 0 ? `执行 ${task.runCount} 次` : "还没执行过");
  if (task.lastStopReason !== null && task.lastStopReason !== "") {
    parts.push(`上次停在「${task.lastStopReason}」`);
  }
  if (task.budget === null) parts.push("未设预算");
  const text = parts.join(" · ");
  // The line is clipped on purpose; `title` keeps the whole sentence reachable on hover.
  return el("div", { class: "task-foot", text, title: text });
}

function baseName(path: string): string {
  const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return cut >= 0 ? path.slice(cut + 1) : path;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/** `clock` = the row is long enough that minutes matter ("0:45 / 10:00" beats "45.2s / 600s"). */
function formatDuration(ms: number, clock: boolean): string {
  if (!clock) return `${(ms / 1000).toFixed(1)}s`;
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.round((ms % 60_000) / 1000);
  return `${minutes}:${String(seconds).padStart(2, "0")}`;
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
  // The human's own words stay plain text: their input is not reinterpreted.
  if (message.role === "user") {
    return el("div", { class: "msg me" }, el("div", { class: "bubble", text: message.content }));
  }

  // The model writes Markdown, so paint Markdown — leaving `**` and `- ` on screen isn't
  // "faithful", it's unrendered. Still DOM-only: see markdown.ts.
  const bubble = el("div", { class: "bubble md" }, renderMarkdown(message.content));

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
