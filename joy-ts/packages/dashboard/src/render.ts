// The functions that paint data into DOM.
//
// Mostly pure functions: give data, get back a node. main.ts only manages "when to repaint",
// not "what it looks like" — when the page gets ugly, look at this one file.

import { renderMarkdown, stripToolFold } from "./markdown";
import { t } from "./i18n";
import type { McpRow, McpView, ScheduleJob, SchedulesView, SkillRow, SkillsView } from "./api";
import type {
  AgentTaskView,
  ArtifactView,
  AcceptanceView,
  BudgetView,
  CheckView,
  ContextUsage,
  DashboardData,
  LedgerEntryView,
  LedgerTurnView,
  MemoryPageSummary,
  MemoryPageView,
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
  onTask: (task: AgentTaskView, action: "start" | "wait" | "input" | "operations" | "suspend" | "resume" | "cancel") => void,
  onInbox: () => void,
): DocumentFragment {
  const { config } = data;
  const fragment = document.createDocumentFragment();

  // The task card leads the overview when there is anything on it: what the loop is
  // chasing, what the model ticked off — the two answers to "what is it doing now".
  if (data.todos !== undefined || data.task != null) {
    const emptyTodos: TodoListView = { revision: 0n, items: [] };
    fragment.append(
      renderTaskCard(data.todos ?? emptyTodos, data.task, data.taskArtifacts ?? [], onTask),
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
      [t("会话"), data.sessions.length],
      [t("事实"), data.facts.length],
      [t("情景"), data.episodes.length],
    ]),
  );

  fragment.append(
    section(
      t("怎么想的"),
      el(
        "dl",
        { class: "rows" },
        ...row(t("小模型"), config.smallModel),
        ...row(t("工作记忆"), t("最近 {historyTurns} 轮", { historyTurns: config.historyTurns })),
        ...row(t("提炼频率"), t("每 {consolidateEvery} 轮", { consolidateEvery: config.consolidateEvery })),
        ...row(t("检索条数"), String(config.retrievalTopK)),
        ...row(t("最多迭代"), String(config.maxIterations)),
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
      t("最近会话（当前：{session}）", { session: session }),
      data.sessions.length === 0
        ? muted(t("还没有会话。说第一句话就有了。"))
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
      text: data.inbox.unseen > 0 ? t("收件箱 · {unseen} 封等你处理", { unseen: data.inbox.unseen }) : t("收件箱"),
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
        text: t("处理这些邮件"),
        onclick: onInbox,
      }),
    );
  }
  block.append(
    el("p", {
      class: "connector-note",
      text: t("只看不标已读；处理是把它们交给一个回合，不会替你回信。"),
    }),
  );
  return block;
}

/// Connectors: one quiet row each — name, whether it can actually send, allowlist width.
/// Read-only on purpose: the switch lives in connectors.json, a file only the human edits.
function renderConnectors(data: DashboardData): HTMLElement {
  const names: Record<string, string> = { email: t("邮件"), feishu: t("飞书") };
  const block = el("div", { class: "block connectors" }, el("h3", { text: t("连接器") }));
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
          text: connector.live ? t("可发送") : t("草稿"),
        }),
        el("span", {
          class: "connector-allow",
          text:
            connector.allowRecipients === 0
              ? t("白名单空 —— 谁都不能收")
              : t("白名单 {allowRecipients} 个", { allowRecipients: connector.allowRecipients }),
        }),
      ),
    );
  }
  block.append(
    el("p", {
      class: "connector-note",
      text: t("开关在 connectors.json 里 —— 只有人能改；实际发送每次都会问。"),
    }),
  );
  return block;
}

/**
 * The task card: the task (which is also where a goal lives) plus the model's own todo sheet,
 * as the human sees them. Both come from `DashboardData` (task/todos, filled only when the page
 * names a session) — this is where "what is the agent actually working on" lives.
 */
export function renderTaskCard(
  todos: TodoListView,
  task: AgentTaskView | undefined,
  artifacts: ArtifactView[],
  onTask: (task: AgentTaskView, action: "start" | "wait" | "input" | "operations" | "suspend" | "resume" | "cancel") => void,
): DocumentFragment {
  const fragment = document.createDocumentFragment();
  const block = el("div", { class: "block taskcard" });

  // The task leads: a goal that survives restarts is a noun with a status, not a condition.
  // Pause/continue are **human switches** — the card is the only place they exist.
  const statusText: Record<string, string> = {
    pending: t("等待开始"),
    running: t("进行中"),
    suspended: t("已暂停"),
    // Not a failure: the machine measured something it must not overrule, so a person looks.
    needs_attention: t("需要你确认"),
    done: t("已完成"),
    failed: t("未达成"),
    cancelled: t("已取消"),
  };
  if (task != null) {
    const live = task.status === "running" || task.status === "pending";
    const automaticWait = task.status === "suspended" &&
      ["waiting for background jobs", "waiting for authorized external input"].includes(task.suspendReason ?? "");
    const resumable = task.status === "needs_attention" || (task.status === "suspended" && !automaticWait);
    const open = live || resumable || automaticWait;
    block.append(
      el("h3", { text: t("任务") }),
      el(
        "div",
        { class: "task-goal" },
        el("span", { class: "task-goal-text", text: task.title }),
        el("span", {
          class: `tag task-state st-${task.status}`,
          text:
            (task.status === "done" && task.acceptance.length === 0
              ? t("已产出，未核验")
              : task.status === "suspended" && task.suspendReason === "waiting for authorized external input" ? t("等待资料")
              : task.status === "suspended" && task.suspendReason === "waiting for background jobs" ? t("等待后台作业")
              : task.status === "pending" && task.dispatchState === "pending" ? t("排队中") : (statusText[task.status] ?? task.status)) +
            (task.goalCondition !== null
              ? t(" · 第 {round}/{max} 轮", { round: task.roundsUsed, max: task.maxRounds })
              : ""),
        }),
      ),
      task.suspendReason !== null
        ? el("div", { class: "task-note", text: task.suspendReason })
        : document.createDocumentFragment(),
      ...(task.inputCondition ? [el("div", { class: "task-note" },
        el("p", { text: task.inputCondition.description }),
        el("p", { text: task.inputCondition.eventId ? t("资料已收到") : t("资料尚未到达") }),
        el("p", { text: task.inputCondition.source === "manual" ? t("由你提供") : t("来源：{source}", { source: task.inputCondition.source }) }),
        ...(task.inputCondition.expiresAt ? [el("p", { text: t("最晚到达：{time}", { time: new Date(task.inputCondition.expiresAt).toLocaleString() }) })] : []),
        ...(task.inputCondition.data !== undefined ? [el("details", {}, el("summary", { text: t("查看资料") }),
          el("pre", { class: "task-input-data", text: typeof task.inputCondition.data === "string" ? task.inputCondition.data : JSON.stringify(task.inputCondition.data, null, 2) }))] : []),
      )] : []),
            // ---- Acceptance: one row per condition; a tick when it passed, a cross plus the
            // measured result when it did not.
      ...acceptanceBlock(task.acceptance, task.checks),
            // ---- Deliverables: files that really exist, not the model's summary of them.
      ...artifactBlock(artifacts),
            // ---- Budget: only a dimension with a limit gets a bar (an unlimited one just shows
            // what was used).
      ...budgetBlock(task.budget, task.spent),
            // ---- Footnote: how many runs, and where the last one stopped.
      taskFooter(task),
      el(
        "div",
        { class: "task-actions" },
        ...(task.status === "pending" && task.dispatchState === undefined
          ? [el("button", {class: "task-act task-act-go", type: "button", text: t("开始任务"), onclick: () => onTask(task, "start")})]
          : []),
        ...(task.status === "pending" && task.dispatchState === undefined && task.runCount === 0 && !task.inputCondition
          ? [el("button", { class: "task-act", type: "button", text: t("等待资料"), onclick: () => onTask(task, "wait") })] : []),
        ...(open && task.inputCondition?.source === "manual" && !task.inputCondition.eventId
          ? [el("button", { class: "task-act", type: "button", text: t("提供资料"), onclick: () => onTask(task, "input") })] : []),
        ...(live || automaticWait
          ? [
              el("button", {
                class: "task-act",
                type: "button",
                text: t("暂停"),
                onclick: () => onTask(task, "suspend"),
              }),
            ]
          : []),
        ...(resumable && task.runCount > 0
          ? [el("button", { class: "task-act", type: "button", text: t("核对操作"), onclick: () => onTask(task, "operations") })] : []),
        ...(resumable
          ? [
              el("button", {
                class: "task-act task-act-go",
                type: "button",
                text: t("继续"),
                onclick: () => onTask(task, "resume"),
              }),
            ]
          : []),
        ...(open
          ? [
              el("button", {
                class: "task-act task-act-stop",
                type: "button",
                text: t("取消"),
                onclick: () => onTask(task, "cancel"),
              }),
            ]
          : []),
      ),
    );
  }

  if (todos.items.length > 0) {
    block.append(
      el("h3", { text: t("待办") }),
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

/** At most this many condition rows before folding into "{N} more". */
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
        title: check?.artifactSha256 ? t("核验时的文件版本：{hash}", { hash: check.artifactSha256 }) : "",
        // Passed: the evidence ("report.md 12.4 KB"). Failed: the measurement.
        text: check?.detail ?? t("尚未检查"),
      }),
    );
  });
  const rest = acceptance.length - rows.length;
  return [
    el("h3", { text: t("验收") }),
    el("ul", { class: "task-checks" }, ...rows),
    rest > 0 ? el("div", { class: "task-more", text: t("还有 {rest} 条", { rest: rest }) }) : document.createDocumentFragment(),
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
    el("h3", { text: t("交付物") }),
    el("ul", { class: "task-artifacts" }, ...rows),
    rest > 0 ? el("div", { class: "task-more", text: t("还有 {rest} 个", { rest: rest }) }) : document.createDocumentFragment(),
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
  // One unit per row: both numbers must read in the same scale, or "45.2s / 10 min 0 s"
  // happens — two ways of saying time side by side looks like a bug.
  const clock = (budget.wallMs ?? spent.wallMs) >= 60_000;
  const time = (value: number) => formatDuration(value, clock);
  const rows = [
    budgetRow(t("时长"), time(spent.wallMs), spent.wallMs, budget.wallMs, time),
    budgetRow("Token", String(spent.tokens), spent.tokens, budget.tokens, String),
    budgetRow(t("工具调用"), String(spent.tools), spent.tools, budget.tools, String),
  ].filter((row): row is HTMLDivElement => row !== null);
  if (rows.length === 0) return [];
  return [el("h3", { text: t("预算") }), el("div", { class: "task-budget" }, ...rows)];
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

/** "3 runs · last stopped at "acceptance failed: report.md does not exist"" with no budget set */
function taskFooter(task: AgentTaskView): Node {
  const parts: string[] = [];
  parts.push(task.runCount > 0 ? t("执行 {runCount} 次", { runCount: task.runCount }) : t("还没执行过"));
  if (task.lastStopReason !== null && task.lastStopReason !== "") {
    parts.push(t("上次停在「{lastStopReason}」", { lastStopReason: task.lastStopReason }));
  }
  if (task.budget === null) parts.push(t("未设预算"));
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
 * the overview keeps the fuller picture. The host in index.html is already the
 * `<ul class="session-list">`, so this returns `<li>` rows directly (no nested list).
 */
export function sessionList(
  data: DashboardData,
  current: string,
  onPick: (id: string) => void,
  onFork?: (id: string) => void,
): DocumentFragment {
  const fragment = document.createDocumentFragment();
  const sessions = [...data.sessions];
  // A brand-new session has no messages yet, so /api/data cannot list it. Keep it on screen
  // and selected until the first turn gives it a real title — otherwise "new chat" looks broken.
  if (!sessions.some((item) => item.id === current)) {
    const fresh: SessionSummary = {
      id: current,
      title: t("新会话"),
      messages: 0,
      startedAt: null,
      lastAt: null,
    };
    sessions.unshift(fresh);
  }
  if (sessions.length === 0) {
    fragment.append(muted(t("还没有会话")));
    return fragment;
  }
  for (const item of sessions) {
    const li = sessionRow(item, current, onPick, onFork);
    arrowKeyNav(li);
    fragment.append(li);
  }
  return fragment;
}

/**
 * Claude Code resume-list keys: ↑/↓ move the focus along the rows (Enter activates —
 * that part is native to buttons). Bound per row; the handler reaches the whole list
 * through `closest("ul")`, so it works in both surfaces that render these rows.
 */
function arrowKeyNav(li: HTMLElement): void {
  li.addEventListener("keydown", (event) => {
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    const list = li.closest("ul");
    if (list === null) return;
    const rows = [...list.querySelectorAll<HTMLButtonElement>("button.session-row")];
    const at = rows.indexOf(document.activeElement as HTMLButtonElement);
    if (at === -1) return;
    event.preventDefault();
    const step = event.key === "ArrowDown" ? 1 : rows.length - 1;
    const next = rows[(at + step) % rows.length];
    next?.focus();
    next?.scrollIntoView({ block: "nearest" });
  });
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
      el("span", { class: "failure-title", text: t("出错了") }),
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
    t("{iterations} 轮", { iterations: turn.meta.iterations }),
    formatMs(turn.meta.latencyMs),
  ];
  if (turn.usage !== null && turn.usage !== undefined) parts.push(formatUsage(turn.usage));
  if (turn.meta.tools.length > 0) parts.push(t("{length} 次工具", { length: turn.meta.tools.length }));
  if (turn.meta.interrupted === true) parts.push(t("已被你停止"));
  // Route reason only speaks when a choice/fallback actually happened (empty in fixed/active).
  if (turn.meta.routeReason) parts.push(t("路由 {routeReason}", { routeReason: turn.meta.routeReason }));
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
    el("p", { class: "welcome-title", text: t("你好，我是 Joy") }),
    el("p", {
      class: "welcome-sub",
      text: t("把要做的事说清楚就行 —— 长任务会一步步来，需要你拍板时会停下来问。"),
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
  // "faithful", it's unrendered. Still DOM-only: see markdown.ts. The stored text carries
  // a model-only fold (see stripToolFold); the human reads the trace capsules instead.
  const bubble = el("div", { class: "bubble md" }, renderMarkdown(stripToolFold(message.content)));

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
          ? t("检索「{q}」", { q: gate.query ?? "" })
          : t("跳过记忆（{reason}）", { reason: gate.reason }),
        "gate",
      ),
    );
  }

  for (const tool of meta.tools) {
    chips.push(
      chip(
        t("工具 · {tool} {status}", {
          tool: tool.tool,
          status: tool.status === "ok" ? t("好了") : t("出错"),
        }),
        "tool",
      ),
    );
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
    parts.unshift(meta.model, t("{iterations} 轮", { iterations: meta.iterations }), formatMs(meta.latencyMs));
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
 * One session row — **exactly one line**, like the Claude Code / Codex / WorkBuddy
 * sidebars: title on the left (ellipsis when it overflows), relative time on the
 * right, nothing else. No second line, no card box; hover and the current selection
 * are the only backgrounds. Details live in the tooltip, not on the row.
 *
 * The inner element is a `<button>` rather than a click on `<li>`: something clickable
 * and hoverable should be a button; only then keyboard and screen readers recognize it.
 */
function sessionRow(
  item: SessionSummary,
  current: string,
  onPick: (id: string) => void,
  onFork?: (id: string) => void,
): HTMLElement {
  const fresh = item.messages === 0;
  const button = el(
    "button",
    {
      class: item.id === current ? "session-row current" : "session-row",
      type: "button",
      title: fresh
        ? t("新会话「{id}」—— 说第一句就有了", { id: item.id })
        : t("「{title}」 · {messages} 条消息 · {id}", { title: item.title, messages: item.messages, id: item.id }),
    },
    el(
      "span",
      { class: "row-title" },
            // Only the inner layer is translated: the outer one clips at the edge, the inner one
            // owns the ellipsis and the scrolling.
      el("span", { class: "row-title-text", text: item.title }),
    ),
    el("span", { class: "row-when", text: fresh ? "" : formatWhen(item.lastAt) }),
  );
  button.addEventListener("click", () => onPick(item.id));
  marqueeOnHover(button);
  const li = el("li", {}, button);
  // A branch carries its origin as a quiet mark, not a separate list: the sidebar stays one
  // flat list of conversations at one visual density.
  if (item.forkedFrom !== undefined) {
    li.classList.add("forked");
    button.title = t("{title} · 分支自「{forkedFrom}」", { title: button.title, forkedFrom: item.forkedFrom });
  }
  if (onFork !== undefined) {
    // The entry point lives on the row (hover reveals it) instead of a permanent button: a
    // sidebar where every row has two controls reads as a toolbar, not a list.
    const fork = el("button", {
      class: "session-fork",
      type: "button",
      text: "⑂",
      title: item.forkedFrom === undefined ? t("从这段对话开一个分支") : t("再开一个分支"),
      "aria-label": t("从「{title}」开一个分支", { title: item.title }),
    });
    fork.addEventListener("click", (event) => {
      event.stopPropagation();
      onFork(item.id);
    });
    li.append(fork);
  }
  return li;
}

/**
 * Hover a truncated title for 500ms and it scrolls horizontally so the whole
 * text becomes readable — the marquee pattern: scroll to the end, hold a beat,
 * scroll back, repeat while the pointer stays. Leave, and it snaps back
 * instantly. Titles that fit never move; reduced-motion users never see it
 * move at all (the tooltip already carries the full text).
 */
function marqueeOnHover(row: HTMLElement): void {
  const box = row.querySelector<HTMLElement>(".row-title");
  const text = row.querySelector<HTMLElement>(".row-title-text");
  if (box === null || text === null) return;

  const SPEED = 40; // px/s: easy to follow, not so slow that it feels stuck
  const DELAY = 500;
  let timer: number | null = null;
  let running = false;

  const stop = (): void => {
    if (timer !== null) {
      window.clearTimeout(timer);
      timer = null;
    }
    running = false;
    text.getAnimations().forEach((animation) => animation.cancel());
    text.classList.remove("scrolling");
  };

  const play = async (): Promise<void> => {
    if (running || !row.isConnected) return;
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const distance = text.scrollWidth - box.clientWidth;
    if (distance <= 0) return; // a title that was not clipped stays put
    running = true;
    text.classList.add("scrolling"); // drop the ellipsis so the sliding words can show
    const hold = (ms: number) =>
      new Promise<void>((resolve) => window.setTimeout(resolve, ms));
    try {
      while (running && row.isConnected) {
        const out = text.animate(
          [{ transform: "translateX(0)" }, { transform: `translateX(${-distance}px)` }],
          { duration: (distance / SPEED) * 1000, easing: "linear", fill: "forwards" },
        );
        await out.finished;
        await hold(1000); // pause at the end so the last words can be read
        const back = text.animate(
          [{ transform: `translateX(${-distance}px)` }, { transform: "translateX(0)" }],
          { duration: Math.min((distance / SPEED) * 1000, 600), easing: "ease-in-out", fill: "forwards" },
        );
        await back.finished;
        await hold(500);
      }
    } catch {
            // cancel() makes finished reject — that is stop()'s path, not an error.
    } finally {
      if (!running) text.classList.remove("scrolling");
    }
  };

  box.addEventListener("mouseenter", () => {
    timer = window.setTimeout(() => void play(), DELAY);
  });
  box.addEventListener("mouseleave", stop);
}

// --- Formatting ---------------------------------------------------------------

/** ISO time → "just now / 12 minutes ago / 09:31 / 3 Aug". */
export function formatWhen(iso: string | null | undefined): string {
  if (iso === null || iso === undefined || iso === "") return "—";
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return iso;

  const minutes = (Date.now() - at.getTime()) / 60_000;
  if (minutes < 1) return t("刚刚");
  if (minutes < 60) return t("{n} 分钟前", { n: Math.floor(minutes) });
  if (minutes < 60 * 24) return clock(at);
  return t("{month} 月 {day} 日", { month: at.getMonth() + 1, day: at.getDate() });
}

/** The header timestamp: when the data is from. */
export function formatStamp(iso: string): string {
  const at = new Date(iso);
  return Number.isNaN(at.getTime()) ? iso : t("{time} 更新", { time: clock(at) });
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
    return Array.isArray(todos) ? t("{length} 项", { length: todos.length }) : t("看一眼");
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

// --- Capabilities: skills / MCP connectors ---------------------------------------
//
// Both panels share one row style: a name, one line of description, a few small tags. The
// only difference is that built-in rows are greyed out and carry no buttons — they ship with
// the installation (JOY_SKILL_DIRS / JOY_MCP_CONFIG), and removing one from this machine's
// interface would break every machine that shares it.

/** The skill list: what Joy knows how to do, and which rows belong to the human. */
export function renderSkills(
  view: SkillsView,
  onAdd: () => void,
  onRemove: (skill: SkillRow) => void,
): DocumentFragment {
  const fragment = document.createDocumentFragment();
  fragment.append(capHead(t("技能 · {length} 个", { length: view.skills.length }), t("＋ 添加技能"), onAdd));

  if (view.error !== null) {
    fragment.append(el("div", { class: "failure", text: view.error }));
  }
  if (view.skills.length === 0) {
    fragment.append(
      muted(
        t("还没有技能。技能是一份 SKILL.md：写上名字、用途和做法，Joy 会在相关消息出现时载入它。"),
      ),
    );
  }

  // A dependency that is not in the list cannot fire: say which one, same as the CLI.
  const loaded = view.skills.map((skill) => skill.name);
  for (const skill of view.skills) {
    const missing = skill.dependencies.filter((dep) => !loaded.includes(dep));
    const row = el(
      "div",
      {
        class: `cap-row${skill.source === "builtin" ? " builtin" : ""}`,
        title:
          skill.source === "builtin"
            ? t("「{name}」随 Joy 内置分发（JOY_SKILL_DIRS）—— 只能看，不能改", { name: skill.name })
            : `「${skill.name}」 · ${skill.folder}`,
      },
      el(
        "div",
        { class: "cap-row-head" },
        el("span", { class: "cap-name", text: skill.name }),
        skill.version !== null
          ? el("span", { class: "tag", text: `v${skill.version}` })
          : document.createDocumentFragment(),
        skill.source === "builtin"
          ? el("span", { class: "tag cap-builtin-tag", text: t("内置") })
          : document.createDocumentFragment(),
        skill.source === "user"
          ? el("button", {
              class: "cap-del",
              type: "button",
              text: "×",
              title: t("删除技能「{name}」", { name: skill.name }),
              onclick: () => onRemove(skill),
            })
          : document.createDocumentFragment(),
      ),
      el("p", { class: "cap-desc", text: skill.description }),
      capMeta([
        skill.allowModelInvocation ? null : t("只走 $名字 显式引用"),
        skill.schedule !== null ? t("定时 {schedule}", { schedule: skill.schedule }) : null,
        missing.length > 0
          ? t("依赖缺失：{list}", { list: missing.join(", ") })
          : null,
      ]),
    );
    fragment.append(row);
  }

  fragment.append(
    el("p", {
      class: "cap-note",
      text: t("内置技能只读；自己加在 <home>/skills/ 下，下一轮就生效，不用重启。"),
    }),
  );
  return fragment;
}

/** The MCP connector list: what Joy is wired to, and how each one authenticates. */
export function renderMcp(
  view: McpView,
  onAdd: () => void,
  onRemove: (server: McpRow) => void,
): DocumentFragment {
  const fragment = document.createDocumentFragment();
  fragment.append(capHead(t("MCP 连接器 · {length} 个", { length: view.servers.length }), t("＋ 添加连接器"), onAdd));

  if (view.error !== null) {
    fragment.append(el("div", { class: "failure", text: view.error }));
  }
  if (view.servers.length === 0) {
    fragment.append(
      muted(
        t("还没有连接器。连上一个 MCP 服务器，它的工具就并进 Joy 的工具表（<服务器>_<工具>）。"),
      ),
    );
  }

  for (const server of view.servers) {
    const target =
      server.kind === "url"
        ? (server.url ?? "")
        : [server.command ?? "", ...server.args].join(" ").trim();
    const auth: Record<string, string> = {
      none: t("无鉴权"),
      auth_env: t("鉴权：{env}（环境变量，值以 Bearer 发出）", { env: server.authEnv ?? "" }),
      oauth: t("浏览器 OAuth（joy mcp login）"),
    };
    fragment.append(
      el(
        "div",
        {
          class: `cap-row${server.source === "builtin" ? " builtin" : ""}`,
          title:
            server.source === "builtin"
              ? t("「{name}」由系统内置配置提供（JOY_MCP_CONFIG）—— 只能看，不能改", { name: server.name })
              : `「${server.name}」 · ${target}`,
        },
        el(
          "div",
          { class: "cap-row-head" },
          el("span", { class: "cap-name", text: server.name }),
          el("span", {
            class: "tag",
            text: server.kind === "url" ? "HTTP" : "stdio",
          }),
          server.source === "builtin"
            ? el("span", { class: "tag cap-builtin-tag", text: t("内置") })
            : document.createDocumentFragment(),
          server.source === "user"
            ? el("button", {
                class: "cap-del",
                type: "button",
                text: "×",
                title: t("删除连接器「{name}」", { name: server.name }),
                onclick: () => onRemove(server),
              })
            : document.createDocumentFragment(),
        ),
        el("code", { class: "cap-target", text: target }),
        capMeta([auth[server.auth] ?? server.auth]),
      ),
    );
  }

  fragment.append(
    el("p", {
      class: "cap-note",
      text: t("内置配置只读；改动落在 <home>/mcp.json，重启 Joy 后生效 —— 连接发生在启动时。"),
    }),
  );
  return fragment;
}

/** Panel header: the title with a count, and the one primary action. */
function capHead(title: string, action: string, onAdd: () => void): HTMLElement {
  return el(
    "div",
    { class: "cap-head" },
    el("p", { class: "hint", text: title }),
    el("button", { class: "approve", type: "button", text: action, onclick: onAdd }),
  );
}

/** The small tag row under a row's name. Null entries are dropped, not rendered empty. */
function capMeta(items: (string | null)[]): HTMLElement {
  const row = el("div", { class: "cap-meta" });
  for (const item of items) {
    if (item === null || item === "") continue;
    row.append(el("span", { class: "cap-meta-item", text: item }));
  }
  return row;
}

/**
 * The scheduled-jobs list: when it fires, what it does, and where the result goes.
 *
 * A row that came from a skill's `schedule:` line is greyed and has no delete button — the
 * job **is** that skill, and removing it here would either do nothing or lie. The "From a skill"
 * tag plus the title attribute say where to go instead.
 */
export function renderSchedules(
  view: SchedulesView,
  onAdd: () => void,
  onRemove: (job: ScheduleJob) => void,
): DocumentFragment {
  const fragment = document.createDocumentFragment();
  fragment.append(capHead(t("定时任务 · {length} 个", { length: view.jobs.length }), t("＋ 添加任务"), onAdd));

  if (view.error !== null) {
    fragment.append(el("div", { class: "failure", text: view.error }));
  }
  if (view.jobs.length === 0) {
    fragment.append(
      muted(
        t("还没有定时任务。加一个：什么时候（cron，如 0 8 * * 1-5）、做什么（一句话）。到点它会在自己的会话里跑一轮，结果写进 outbox。"),
      ),
    );
  }

  for (const job of view.jobs) {
    const row = el(
      "div",
      {
        class: `cap-row${job.source === "skill" ? " builtin" : ""}`,
        title:
          job.source === "skill"
            ? t("「{name}」的定时来自技能 {skill} 的 SKILL.md —— 改那里，不在这里", {
                name: job.name,
                skill: job.skill ?? job.name,
              })
            : t("「{name}」写在 <home>/schedules.json", { name: job.name }),
      },
      el(
        "div",
        { class: "cap-row-head" },
        el("span", { class: "cap-name", text: job.name }),
        el("span", { class: "tag sched-cron", text: job.cron }),
        job.source === "skill"
          ? el("span", { class: "tag cap-builtin-tag", text: t("随技能") })
          : document.createDocumentFragment(),
        job.source === "file"
          ? el("button", {
              class: "cap-del",
              type: "button",
              text: "×",
              title: t("删除定时任务「{name}」", { name: job.name }),
              onclick: () => onRemove(job),
            })
          : document.createDocumentFragment(),
      ),
      el("p", { class: "cap-desc", text: job.prompt }),
      capMeta([
        job.nextRun !== null ? t("下次 {nextRun}", { nextRun: job.nextRun }) : job.nextRunNote,
        job.lastFiredMinute !== null
          ? t("上次 {lastFiredMinute}", { lastFiredMinute: job.lastFiredMinute })
          : t("还没跑过"),
        job.claimedMinute !== null ? t("上次被中断，重启时按「错过是否补跑」处理") : null,
        t("投递 {delivery}", { delivery: job.delivery }),
        job.catchUp ? null : t("错过不补跑"),
      ]),
    );
    fragment.append(row);
  }

  fragment.append(
    el("p", {
      class: "cap-note",
      text: t("到点执行的是 `joy schedule` 这个常驻进程 —— 它没在跑就不会触发（用系统 cron 驱动 `joy gather` 也一样）。输出落在 <home>/outbox/，不进聊天。"),
    }),
  );
  return fragment;
}

// ---- The memory center ---------------------------------------------------------------
//
// Three read-only views over one drawer. Density over decoration: the ledger is "one turn
// per row, open it for structure", the bundle is a book page with its provenance on top,
// the meter is one band. No charts, no cards inside cards.

/** Local `HH:MM` from the ledger's ISO stamp; the raw value when it will not parse. */
function clockOf(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  return date.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
}

/**
 * The ledger: one row per turn, opening in place.
 *
 * A folded row is marked, not hidden: after the backfill those turns genuinely only have a
 * summary, and pretending they are complete would make the ledger lie about its own fidelity.
 */
export function renderLedger(
  turns: LedgerTurnView[],
  entries: LedgerEntryView[],
  openTurn: string | null,
  onOpen: (turnId: string) => void,
): DocumentFragment {
  const fragment = document.createDocumentFragment();
  if (turns.length === 0) {
    fragment.append(
      muted(t("这个会话还没有记录。第一轮说完话，账本就会有第一行 —— 只追加，不改写。")),
    );
    return fragment;
  }
  const head = el(
    "div",
    { class: "mem-head" },
    el("p", { class: "hint", text: t("账本 · {length} 轮", { length: turns.length }) }),
    el("span", { class: "mem-note", text: t("只追加 · 可重建") }),
  );
  fragment.append(head);

  for (const turn of turns) {
    const folded = turn.fidelity !== "exact";
    const row = el("button", {
      class: `mem-row${openTurn === turn.turnId ? " open" : ""}`,
      type: "button",
      title: folded ? t("这一轮的工具输出只保留了摘要（回填而来，原文已不可重建）") : turn.preview,
      onclick: () => onOpen(turn.turnId),
    });
    row.append(
      el("span", { class: "mem-time", text: clockOf(turn.at) }),
      el("span", { class: "mem-preview", text: turn.preview === "" ? t("（空）") : turn.preview }),
      el(
        "span",
        { class: "mem-tags" },
        el("span", { class: "tag", text: t("{entries} 条", { entries: turn.entries }) }),
        turn.consolidated ? el("span", { class: "tag", text: t("已整理") }) : document.createDocumentFragment(),
        folded ? el("span", { class: "tag mem-folded", text: t("折叠") }) : document.createDocumentFragment(),
      ),
    );
    fragment.append(row);

    if (openTurn !== turn.turnId) continue;
    const detail = el("div", { class: "mem-entries" });
    if (entries.length === 0) {
      detail.append(muted(t("这一轮没有可展开的条目。")));
    }
    for (const entry of entries) {
      detail.append(
        el(
          "div",
          { class: "mem-entry" },
          el("span", { class: `mem-kind mem-kind-${entry.kind}`, text: entry.kind }),
          el("span", { class: "mem-entry-text", text: entry.preview }),
          entry.fidelity !== "exact"
            ? el("span", { class: "tag mem-folded", text: t("折叠") })
            : document.createDocumentFragment(),
        ),
      );
    }
    fragment.append(detail);
  }
  return fragment;
}

/**
 * The bundle: catalog on the left, one page on the right, provenance across the top.
 *
 * `sources` are clickable on purpose — the whole point of a derived knowledge page is that
 * you can walk back to the rows it was distilled from.
 */
export function renderWiki(
  pages: MemoryPageSummary[],
  current: MemoryPageView | null,
  onPick: (slug: string) => void,
  onSource: (source: string) => void,
  onRebuild: () => void,
): DocumentFragment {
  const fragment = document.createDocumentFragment();
  if (pages.length === 0) {
    fragment.append(
      muted(t("记忆库还是空的。它由后台整理写出来 —— 攒够几轮之后就会有自己的目录。")),
    );
    return fragment;
  }

  const catalog = el("div", { class: "mem-catalog" });
  catalog.append(el("p", { class: "hint", text: t("目录 · {length} 页", { length: pages.length }) }));
  for (const page of pages) {
    catalog.append(
      el("button", {
        class: `mem-page-row${current !== null && current.slug === page.slug ? " open" : ""}`,
        type: "button",
        title: page.summary,
        onclick: () => onPick(page.slug),
      }, el("span", { class: "mem-page-title", text: page.title }), el("span", { class: "tag", text: page.kind })),
    );
  }
  fragment.append(catalog);

  if (current === null) {
    fragment.append(muted(t("点左边的一页看看 —— 每页都带着它从哪几轮账本里来的。")));
    return fragment;
  }

  const sources = el("div", { class: "mem-sources" });
  if (current.sources.length === 0) {
    sources.append(el("span", { class: "mem-note", text: t("这页没有记出处") }));
  }
  for (const source of current.sources) {
    sources.append(
      el("button", {
        class: "chip note mem-source",
        type: "button",
        text: source,
        title: t("跳到账本里那一轮"),
        onclick: () => onSource(source),
      }),
    );
  }

  fragment.append(
    el(
      "article",
      { class: "mem-page" },
      el(
        "header",
        { class: "mem-page-head" },
        el("h1", { class: "mem-page-name", text: current.title }),
        el("span", { class: "tag", text: current.kind }),
      ),
      el("p", { class: "mem-page-summary", text: current.summary }),
      sources,
      el("div", { class: "mem-page-body" }, renderMarkdown(current.body)),
      el("button", {
        class: "ghost mem-rebuild",
        type: "button",
        text: t("重新生成"),
        title: t("换更强的模型后可以整页重跑（会先在后台重读来源轮次）"),
        onclick: onRebuild,
      }),
    ),
  );
  return fragment;
}

/**
 * The context meter: one band, six sections, two numbers underneath.
 *
 * A notice appears at the configured threshold — a sentence at the end of the bar, not a
 * dialog: being close to the window's edge is information, not an emergency.
 */
export function renderContext(
  usage: ContextUsage | null,
  onCompact: () => void,
): DocumentFragment {
  const fragment = document.createDocumentFragment();
  if (usage === null) {
    fragment.append(muted(t("还没有测量 —— 这个会话跑过一轮之后就有数了。")));
    return fragment;
  }
  const total = usage.parts.reduce((sum, part) => sum + Math.max(part.tokens, 0), 0);
  const band = el("div", { class: "mem-band", role: "img", "aria-label": t("上下文占用分层条") });
  for (const part of usage.parts) {
    if (part.tokens <= 0 || total === 0) continue;
    band.append(
      el("span", {
        class: `mem-band-seg seg-${part.name}`,
        style: `flex-grow: ${part.tokens}`,
        title: `${part.name} · ${part.tokens} token`,
      }),
    );
  }

  const percent = Math.round(usage.ratio * 100);
  const numbers = el(
    "div",
    { class: "mem-numbers" },
    el("span", { text: `${usage.usedTokens} / ${usage.budgetTokens} token · ${percent}%` }),
    el(
      "span",
      { class: `mem-percent${percent >= 85 ? " hot" : ""}` },
      el("span", { class: "mem-key" }),
      el("span", { text: t("灵魂 · 记忆 · 技能 · 工具 · 历史 · 摘要（hover 看明细）") }),
    ),
  );

  fragment.append(
    el(
      "div",
      { class: "mem-meter" },
      el("p", { class: "hint", text: t("上下文占用") }),
      band,
      numbers,
      usage.notice !== null
        ? el("p", { class: "mem-warn", text: usage.notice })
        : document.createDocumentFragment(),
      el("button", {
        class: "ghost mem-compact",
        type: "button",
        text: t("现在压缩"),
        title: t("把较早的轮次折进摘要，保留最近两轮"),
        onclick: onCompact,
      }),
    ),
  );
  return fragment;
}
