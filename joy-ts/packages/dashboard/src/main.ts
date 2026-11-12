// The dashboard's main program: on boot, pull back the current session's history, then wait for input.
//
// The page itself is **stateless**: sessions, memory, and config all live on the Rust side
// (behind app-server is state.db). The only temporary view state here is "which session is being
// viewed" and "whether a turn is streaming"; a refresh returns to defaults. In other words, if the
// browser crashes or the tab closes, nothing is lost — that's the most valuable part of this layering.

import type {
  AgentTaskView,
  ContextUsage,
  LedgerEntryView,
  LedgerTurnView,
  MemoryPageSummary,
  MemoryPageView,
  TurnAttachment,
  TurnCompletedNotification,
  TurnView,
} from "./protocol";
import type {
  McpRow,
  McpServerSpec,
  McpView,
  Outcome,
  ScheduleJob,
  ScheduleSpec,
  SchedulesView,
  SkillRow,
  SkillsView,
} from "./api";
import { createTimeline } from "./timeline";
import { showLogin } from "./login";
import { startSync } from "./sync";
import {
  compactSession,
  createFolder,
  createSession,
  fetchContextUsage,
  fetchDashboard,
  fetchFs,
  fetchLedger,
  fetchLedgerTurn,
  fetchMemoryIndex,
  fetchMemoryPage,
  fetchSkillStats,
  fetchMcp,
  fetchMessages,
  fetchSchedules,
  fetchRecents,
  fetchSessionMeta,
  fetchSkills,
  fetchTurns,
  followUpTurn,
  installSkill,
  interruptTurn,
  recordRecent,
  removeMcpServer,
  removeSchedule,
  removeSkill,
  respondApproval,
  saveMcpServer,
  saveSchedule,
  steerTurn,
  summarizeInbox,
  talk,
  taskAction,
  updateSession,
  uploadFile,
} from "./api";
import { renderMarkdown, StreamMarkdown } from "./markdown";
import {
  attachmentStrip,
  chip,
  el,
  elapsedSince,
  failure,
  failureOf,
  formatMs,
  formatStamp,
  messageRow,
  muted,
  renderContext,
  renderLedger,
  renderMcp,
  renderSchedules,
  renderOverview,
  renderSkills,
  renderWiki,
  sessionList,
  toolSummary,
  turnFooter,
  welcome,
} from "./render";

/**
 * Which session is currently being viewed.
 *
 * A session is not stored in the browser — just a label on chat_log — so "switching" means
 * changing this variable and re-fetching the history, nothing more. The default session is
 * a good thing: `joy` in the terminal talks on the same line too, so they connect.
 */
let current = "default";

/** Cursor for paging back through older messages. `null` = already at the earliest. */
let olderCursor: string | null = null;

/** How often the panels refresh themselves. All local RPC, a few microseconds, no cost. */
const POLL_MS = 15_000;

const dom = {
  messages: must("messages"),
  composer: must<HTMLFormElement>("composer"),
  input: must<HTMLTextAreaElement>("input"),
  send: must<HTMLButtonElement>("send"),
  /** 只在有轮次可跟时出现：按下 = 发送落成下一轮，而不是补充这一轮。 */
  queueToggle: must<HTMLButtonElement>("queue-toggle"),
  refresh: must<HTMLButtonElement>("refresh"),
  older: must<HTMLButtonElement>("older"),
  overview: must("overview"),
  status: must("status"),
  /** A button, never an input: the human picks a folder, and that pick is the write boundary. */
  cwd: must<HTMLButtonElement>("cwd"),
  cwdLabel: must<HTMLElement>("cwd-label"),
  progress: must<HTMLButtonElement>("progress"),
  attach: must<HTMLButtonElement>("attach"),
  file: must<HTMLInputElement>("file"),
  staged: must<HTMLElement>("staged"),
  session: must("session"),
  stamp: must("stamp"),
  sessions: must("sessions"),
  newSession: must<HTMLButtonElement>("new-session"),
  drawer: must("drawer"),
  drawerClose: must<HTMLButtonElement>("drawer-close"),
  // 抽屉的三个面板 + 侧栏能力中心入口与计数。
  tabOverview: must<HTMLButtonElement>("tab-overview"),
  tabSkills: must<HTMLButtonElement>("tab-skills"),
  tabMcp: must<HTMLButtonElement>("tab-mcp"),
  tabMemory: must<HTMLButtonElement>("tab-memory"),
  tabSchedule: must<HTMLButtonElement>("tab-schedule"),
  panelOverview: must("panel-overview"),
  panelSkills: must("panel-skills"),
  panelMcp: must("panel-mcp"),
  panelMemory: must("panel-memory"),
  panelSchedule: must("panel-schedule"),
  skillsBody: must("skills-body"),
  mcpBody: must("mcp-body"),
  schedulesBody: must("schedules-body"),
  memSegLedger: must<HTMLButtonElement>("mem-seg-ledger"),
  memSegWiki: must<HTMLButtonElement>("mem-seg-wiki"),
  memSegContext: must<HTMLButtonElement>("mem-seg-context"),
  memLedger: must("mem-ledger"),
  memWiki: must("mem-wiki"),
  memContext: must("mem-context"),
  capSkills: must<HTMLButtonElement>("cap-skills"),
  capMcp: must<HTMLButtonElement>("cap-mcp"),
  capSchedules: must<HTMLButtonElement>("cap-schedules"),
  capSkillsCount: must<HTMLElement>("cap-skills-count"),
  capMcpCount: must<HTMLElement>("cap-mcp-count"),
  capSchedulesCount: must<HTMLElement>("cap-schedules-count"),
};

/**
 * The chosen workspace. It is the only client-side state we keep (localStorage): a person
 * expects a folder picker to remember the folder, and the server already records it in
 * workspaces.json for the "recents" list. Never a path typed by hand.
 */
const WORKSPACE_KEY = "joy.workspace";
let workspace: string | null = localStorage.getItem(WORKSPACE_KEY);
/** Where uploads land. Kept off-screen (config is not the point of this page), but needed to
 *  tell the model where to find a non-image attachment. */
let joyHome = "";

function setWorkspace(path: string | null): void {
  workspace = path;
  if (path === null) {
    localStorage.removeItem(WORKSPACE_KEY);
    dom.cwdLabel.textContent = "选择工作目录";
    dom.cwd.title = "选一个文件夹作为工作目录——它同时也是唯一的可写范围";
  } else {
    localStorage.setItem(WORKSPACE_KEY, path);
    // A folder's name is what a person recognizes; the full path stays in the tooltip.
    dom.cwdLabel.textContent = path.split("/").filter(Boolean).at(-1) ?? path;
    dom.cwd.title = `工作目录：${path}（只有这里能写文件）`;
  }
}

let busy = false;

/**
 * A turn running on **this session, but not on this page** — another tab, the terminal,
 * or a follow-up queued by an earlier turn. `busy` cannot see it: no stream lives here.
 * So the page asks `turn/list`, on open and on every pulse. Its value is that turn's
 * `startedAt`, which lets the statusline say "running for 3 minutes" instead of "running".
 *
 * Sending while this is set still means **steer**: there is a turn to attach to, and
 * asking for a second one would be refused (`SESSION_BUSY`).
 */
let otherTurn: string | null = null;

/** 这一句是「跑完再说」：有轮次在跑时，发送落成**下一轮**而不是补充当前这轮。 */
let queuedMode = false;

/**
 * Staging area ("reference first"): files wait here, uploaded on send with absolute paths
 * injected into the message — referencing and reading are two things; the model decides.
 */
const staged: File[] = [];

/** Object URLs for staged image previews; revoked the moment they leave the stage. */
const stagedUrls = new Map<File, string>();

function previewUrl(file: File): string | undefined {
  if (!file.type.startsWith("image/")) return undefined;
  let url = stagedUrls.get(file);
  if (url === undefined) {
    url = URL.createObjectURL(file);
    stagedUrls.set(file, url);
  }
  return url;
}

function releaseStaged(files: File[]): void {
  for (const file of files) {
    const url = stagedUrls.get(file);
    if (url !== undefined) {
      URL.revokeObjectURL(url);
      stagedUrls.delete(file);
    }
  }
}

function renderStaged(): void {
  dom.staged.hidden = staged.length === 0;
  const nodes = staged.map((file, index) => {
    const url = previewUrl(file);
    if (url !== undefined) {
      const img = el("img", { class: "attach-thumb", src: url, alt: file.name, title: file.name });
      img.onerror = () => img.remove();
      const drop = el("button", {
        class: "thumb-drop",
        text: "×",
        title: "拿掉这个文件",
        type: "button",
      });
      drop.addEventListener("click", () => {
        releaseStaged([file]);
        staged.splice(index, 1);
        renderStaged();
      });
      return el("span", { class: "thumb" }, img, drop);
    }
    const item = el("span", { class: "chip", text: file.name });
    const drop = el("button", {
      class: "ghost",
      text: "×",
      title: "拿掉这个文件",
    });
    drop.addEventListener("click", () => {
      releaseStaged([file]);
      staged.splice(index, 1);
      renderStaged();
    });
    item.append(drop);
    return item;
  });
  dom.staged.replaceChildren(...nodes);
}

/**
 * The drawer's three panels: the overview (what is being worked on, what is remembered)
 * and the capability center's two halves. Skills and MCP connectors are here rather than
 * in a settings page because they are things a human adds and removes, not config to
 * stare at — same reason they get a sidebar entry.
 */
const TAB_IDS = ["overview", "skills", "mcp", "memory", "schedule"] as const;
type TabId = (typeof TAB_IDS)[number];

/** Which panel is showing. Kept so the 进度 button can toggle instead of only opening. */
let currentTab: TabId = "overview";

/** Latest capability data, refreshed with the rest of the screen. */
let skillsView: SkillsView = { skills: [], error: null };
let mcpView: McpView = { servers: [], error: null };
let schedulesView: SchedulesView = { jobs: [], error: null };

function switchTab(tab: TabId): void {
  currentTab = tab;
  // `#overview` is still a bookmark: one URL opens the progress drawer on load.
  if (location.hash !== `#${tab}`) {
    history.replaceState(null, "", `#${tab}`);
  }
  for (const [id, button] of [
    ["overview", dom.tabOverview],
    ["skills", dom.tabSkills],
    ["mcp", dom.tabMcp],
    ["memory", dom.tabMemory],
    ["schedule", dom.tabSchedule],
  ] as const) {
    const active = id === tab;
    button.classList.toggle("active", active);
    button.setAttribute("aria-selected", String(active));
  }
  dom.panelOverview.hidden = tab !== "overview";
  dom.panelSkills.hidden = tab !== "skills";
  dom.panelMcp.hidden = tab !== "mcp";
  dom.panelMemory.hidden = tab !== "memory";
  dom.panelSchedule.hidden = tab !== "schedule";
  // The capability panels paint on open: their data may predate the last refresh.
  if (tab === "skills") {
    paintSkills();
    void loadSkillStats();
  }
  if (tab === "mcp") paintMcp();
  if (tab === "schedule") paintSchedules();
  // The memory center re-reads on open: the ledger grows and the bundle changes under the
  // background worker, so a cached view would be a claim about the past.
  if (tab === "memory") void loadMemory();
  openDrawer();
}

/** The panel drawer slides in from the right; the chat column never squeezes. */
function openDrawer(): void {
  dom.drawer.hidden = false;
}

/** Open the panel named in the URL hash, if it is one we know. */
function openHashPanel(): void {
  const tab = location.hash.slice(1);
  if ((TAB_IDS as readonly string[]).includes(tab)) switchTab(tab as TabId);
}

function closeDrawer(): void {
  dom.drawer.hidden = true;
}

/**
 * How often each skill actually fired — the memory center's answer to "is this skill worth
 * keeping". Telemetry: if the log cannot be read, the panel simply says nothing about it.
 */
let skillHits = new Map<string, number>();

async function loadSkillStats(): Promise<void> {
  try {
    const answer = await fetchSkillStats();
    skillHits = new Map(answer.data.map((stat) => [stat.name, stat.hits]));
  } catch {
    return;
  }
  paintSkills();
}

/** The three most-fired skills, as one line. Silent when nothing has fired yet. */
function hitsLine(): Node[] {
  const rows = [...skillHits.entries()]
    .filter(([, hits]) => hits > 0)
    .sort((left, right) => right[1] - left[1])
    .slice(0, 3);
  if (rows.length === 0) return [];
  const text = rows.map(([name, hits]) => `${name} ${hits} 次`).join("、");
  return [chip(`命中：${text}`, "note")];
}

/** Paint the skill panel from the latest `skillsView`. */
function paintSkills(): void {
  const note = outcomeNote(dom.skillsBody);
  dom.skillsBody.replaceChildren(
    ...(note === null ? [] : [note]),
    ...hitsLine(),
    renderSkills(skillsView, () => void skillAddOverlay(), (skill) => void removeSkillAsk(skill)),
  );
}

/** Paint the MCP panel from the latest `mcpView`. */
function paintMcp(): void {
  const note = outcomeNote(dom.mcpBody);
  dom.mcpBody.replaceChildren(
    ...(note === null ? [] : [note]),
    renderMcp(mcpView, () => void mcpAddOverlay(), (server) => void removeMcpAsk(server)),
  );
}

/** Paint the scheduled-jobs panel from the latest `schedulesView`. */
function paintSchedules(): void {
  const note = outcomeNote(dom.schedulesBody);
  dom.schedulesBody.replaceChildren(
    ...(note === null ? [] : [note]),
    renderSchedules(
      schedulesView,
      () => void scheduleAddOverlay(),
      (job) => removeScheduleAsk(job),
    ),
  );
}

// ---- The memory center ---------------------------------------------------------------
//
// Three views, one state each, no cache: opening a view re-reads it. The ledger only grows,
// the bundle is rewritten under the background worker, and the meter belongs to the last
// turn — a picture painted from memory would be a claim about a past that has moved.

const MEM_SEGS = ["ledger", "wiki", "context"] as const;
type MemSeg = (typeof MEM_SEGS)[number];

let memSeg: MemSeg = "ledger";
let ledgerTurns: LedgerTurnView[] = [];
let ledgerEntries: LedgerEntryView[] = [];
/** Which turn is open. `null` = all rows collapsed. */
let ledgerOpen: string | null = null;
let wikiPages: MemoryPageSummary[] = [];
let wikiCurrent: MemoryPageView | null = null;
let contextUsage: ContextUsage | null = null;
/** One line of context for the current view (a rebuild hint, a "nothing to fold"). */
let memNote: string | null = null;
let memError: unknown = null;

function paintMemory(): void {
  for (const [id, button] of [
    ["ledger", dom.memSegLedger],
    ["wiki", dom.memSegWiki],
    ["context", dom.memSegContext],
  ] as const) {
    const active = id === memSeg;
    button.classList.toggle("active", active);
    button.setAttribute("aria-selected", String(active));
  }
  dom.memLedger.hidden = memSeg !== "ledger";
  dom.memWiki.hidden = memSeg !== "wiki";
  dom.memContext.hidden = memSeg !== "context";

  if (memError !== null) {
    // One failure at a time, on whichever view is showing: a stale error under a fresh
    // view reads as "this is broken now", which is worse than saying nothing.
    const error = memError;
    memError = null;
    const target =
      memSeg === "ledger" ? dom.memLedger : memSeg === "wiki" ? dom.memWiki : dom.memContext;
    target.replaceChildren(failureOf(error));
    return;
  }

  const note = memNote === null ? [] : [chip(memNote, "note")];
  if (memSeg === "ledger") {
    dom.memLedger.replaceChildren(
      renderLedger(ledgerTurns, ledgerEntries, ledgerOpen, (turnId) => void openLedgerTurn(turnId)),
    );
  } else if (memSeg === "wiki") {
    dom.memWiki.replaceChildren(
      ...note,
      renderWiki(
        wikiPages,
        wikiCurrent,
        (slug) => void openMemoryPage(slug),
        (source) => void jumpToSource(source),
        rebuildHint,
      ),
    );
  } else {
    dom.memContext.replaceChildren(renderContext(contextUsage, () => void compactNow()));
  }
}

/** Read the view that is showing, then paint it. */
async function loadMemory(): Promise<void> {
  if (current === "") {
    memError = new Error("还没有选会话 —— 先开一个会话，账本才有得看。");
    paintMemory();
    return;
  }
  try {
    if (memSeg === "ledger") {
      const answer = await fetchLedger(current, 30);
      ledgerTurns = answer.turns;
      ledgerEntries = [];
      ledgerOpen = null;
    } else if (memSeg === "wiki") {
      const answer = await fetchMemoryIndex();
      wikiPages = answer.pages;
      wikiCurrent = null;
    } else {
      const answer = await fetchContextUsage(current);
      contextUsage = answer.usage;
    }
    memError = null;
  } catch (error) {
    memError = error;
  }
  paintMemory();
}

/** Open one turn in place; opening the open one closes it. */
async function openLedgerTurn(turnId: string): Promise<void> {
  if (ledgerOpen === turnId) {
    ledgerOpen = null;
    ledgerEntries = [];
    paintMemory();
    return;
  }
  try {
    const answer = await fetchLedgerTurn(current, turnId);
    ledgerTurns = answer.turns;
    ledgerEntries = answer.entries;
    ledgerOpen = turnId;
    memError = null;
  } catch (error) {
    memError = error;
  }
  paintMemory();
}

async function openMemoryPage(slug: string): Promise<void> {
  try {
    const answer = await fetchMemoryPage(slug);
    if (answer.page === null) {
      memNote = `记忆库里没有「${slug}」这一页 —— 它可能在后台整理时被改名了，刷新目录看看。`;
      wikiCurrent = null;
    } else {
      wikiCurrent = answer.page;
      memNote = null;
    }
    memError = null;
  } catch (error) {
    memError = error;
  }
  paintMemory();
}

/**
 * Walk back from a page's provenance to the ledger rows it came from.
 *
 * Sources read `ledger:<session>/<turn>` (older pages: `ledger:<seq>`). Neither form is a
 * session-scoped `turnId` we can open directly, so the honest move is: switch to the ledger
 * and open the row whose id ends with that tail — or say we could not find it.
 */
async function jumpToSource(source: string): Promise<void> {
  const tail = source.split(/[:/]/).filter(Boolean).at(-1) ?? "";
  memSeg = "ledger";
  memNote = null;
  if (tail === "") {
    await loadMemory();
    return;
  }
  try {
    const answer = await fetchLedger(current, 100);
    ledgerTurns = answer.turns;
    const match = answer.turns.find((turn) => turn.turnId === tail || turn.turnId.endsWith(tail));
    if (match === undefined) {
      ledgerEntries = [];
      ledgerOpen = null;
      memNote = `账本里没有 ${source} 对应的那一轮（可能已被整理进更早的摘要）。`;
      memError = null;
      paintMemory();
      return;
    }
    await openLedgerTurn(match.turnId);
  } catch (error) {
    memError = error;
    paintMemory();
  }
}

/**
 * "Regenerate" is a **command**, not a button that secretly spends a model call.
 *
 * Whole-page regeneration re-reads the source turns with whatever model is configured now;
 * that is `joy memory rebuild`, and it belongs where a person can see the cost.
 */
function rebuildHint(): void {
  memNote = "整页重生成走 CLI：joy memory rebuild --page " + (wikiCurrent?.slug ?? "<slug>");
  paintMemory();
}

/** `POST /api/compact`: fold the older turns in now, keeping the newest few. */
async function compactNow(): Promise<void> {
  try {
    const answer = await compactSession(current);
    memNote =
      answer.coveredTurns === 0
        ? "没有更早的轮次可折 —— 摘要已经跟上了。"
        : `已压缩：摘要现在覆盖 ${answer.coveredTurns} 轮（${answer.summaryTokens} token）。`;
    const usage = await fetchContextUsage(current);
    contextUsage = usage.usage;
    memError = null;
  } catch (error) {
    memError = error;
  }
  paintMemory();
}

/**
 * The last write's outcome on a panel, remembered so the 15s repaint cannot wipe it.
 *
 * A one-shot prepend would vanish on the next poll — "did the delete work?" answered
 * by a line that disappears is worse than no line. Keyed by the panel element, so the
 * two lists never borrow each other's receipt.
 */
const lastOutcome = new WeakMap<
  HTMLElement,
  { ok: true; message: string } | { ok: false; error: unknown }
>();

function outcomeNote(panel: HTMLElement): Node | null {
  const outcome = lastOutcome.get(panel);
  if (outcome === undefined) return null;
  return outcome.ok ? chip(outcome.message, "note") : failureOf(outcome.error);
}

// --- Run state, three kinds -----------------------
//
// "Waiting for you" is static; "Thinking/Replying" gets shimmer + stopwatch; "Working" carries
// the tool name. It distinguishes "stuck" from "running" — a moving stopwatch means the latter.

let statusTimer: number | null = null;
let statusStart = 0;
/** The self-dismiss timer of the crash-recovery notice (see startSync's onActivity). */
let resumeNotice: number | null = null;
/** The running turn's handle (from `turnStarted`) — what the stop button points at. */
let activeTurnId: string | null = null;

function showStatus(kind: "thinking" | "working", label: string): void {
  dom.status.hidden = false;
  dom.status.className = `statusline ${kind}`;
  if (dom.status.querySelector(".status-label") === null) {
    statusStart = Date.now();
    dom.status.replaceChildren(
      el("span", { class: "status-label", text: label }),
      el("span", { class: "status-time" }),
    );
    statusTimer = window.setInterval(() => {
      const time = dom.status.querySelector(".status-time");
      if (time !== null) time.textContent = elapsedSince(statusStart);
    }, 500);
  } else {
    const labelNode = dom.status.querySelector(".status-label");
    if (labelNode !== null) labelNode.textContent = label;
  }
  if (dom.status.className !== `statusline ${kind}`) {
    // Switching from thinking to working: reset the timer start so the stopwatch matches this step.
    statusStart = Date.now();
    dom.status.className = `statusline ${kind}`;
  }
}

function hideStatus(): void {
  dom.status.hidden = true;
  dom.status.replaceChildren();
  activeTurnId = null;
  if (statusTimer !== null) {
    window.clearInterval(statusTimer);
    statusTimer = null;
  }
}

/** Full view for an image: click anywhere or Esc to leave — no chrome, just the picture. */
function openLightbox(src: string, alt: string): void {
  const overlay = el("div", { class: "lightbox", role: "dialog", "aria-label": alt });
  overlay.append(el("img", { src, alt }));
  overlay.addEventListener("click", () => overlay.remove());
  window.addEventListener(
    "keydown",
    (event) => {
      if (event.key === "Escape") overlay.remove();
    },
    { once: true },
  );
  document.body.append(overlay);
}

/** The stop button rides the status line while a turn runs — one click, honest answer. */
function addStopButton(): void {
  if (dom.status.querySelector(".status-stop") !== null) return;
  const stop = el("button", { class: "ghost status-stop", text: "停止", type: "button" });
  stop.addEventListener("click", () => {
    if (activeTurnId === null) return;
    stop.disabled = true;
    void interruptTurn({ turnId: activeTurnId })
      .then((result) => {
        if (!result.interrupted) {
          const label = dom.status.querySelector(".status-label");
          if (label !== null) label.textContent = "（那一轮已经结束了）";
        }
      })
      .catch(() => undefined);
  });
  dom.status.append(stop);
}

/**
 * The sidebar's edge is draggable: grab the border, pull left/right, the width
 * sticks (localStorage). Double-click resets; ←/→ nudge by 24px when the handle
 * is focused. Pointer events cover mouse and touch alike; `setPointerCapture`
 * keeps the drag alive when the cursor outruns the 7px handle.
 */
function setupSidebarResize(): void {
  const handle = must<HTMLElement>("sidebar-resizer");
  const app = document.querySelector<HTMLElement>(".app");
  if (app === null) return;

  const MIN = 180;
  const MAX = 480;
  const DEFAULT = 232;
  const KEY = "joy.sidebar.w";
  const apply = (w: number): void => app.style.setProperty("--sidebar-w", `${Math.round(w)}px`);
  const clamp = (w: number): number => Math.min(MAX, Math.max(MIN, w));

  const saved = Number(localStorage.getItem(KEY));
  if (Number.isFinite(saved) && saved >= MIN && saved <= MAX) apply(saved);

  let dragging = false;
  handle.addEventListener("pointerdown", (event) => {
    dragging = true;
    handle.setPointerCapture(event.pointerId);
    document.body.classList.add("resizing");
  });
  handle.addEventListener("pointermove", (event) => {
    if (dragging) apply(clamp(event.clientX));
  });
  const stop = (): void => {
    if (!dragging) return;
    dragging = false;
    document.body.classList.remove("resizing");
    const w = app.style.getPropertyValue("--sidebar-w");
    if (w !== "") localStorage.setItem(KEY, w);
  };
  handle.addEventListener("pointerup", stop);
  handle.addEventListener("pointercancel", stop);

  handle.addEventListener("dblclick", () => {
    apply(DEFAULT);
    localStorage.removeItem(KEY);
  });
  handle.addEventListener("keydown", (event) => {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
    event.preventDefault();
    const current =
      Number.parseFloat(getComputedStyle(app).getPropertyValue("--sidebar-w")) || DEFAULT;
    const w = clamp(current + (event.key === "ArrowRight" ? 24 : -24));
    apply(w);
    localStorage.setItem(KEY, `${Math.round(w)}px`);
  });
}

async function boot(): Promise<void> {
  dom.refresh.addEventListener("click", () => void refresh());
  dom.older.addEventListener("click", () => void older());
  dom.newSession.addEventListener("click", () => void newSession());
  dom.drawerClose.addEventListener("click", closeDrawer);
  setupSidebarResize();

  // Click a thumbnail anywhere in the flow (or the staging strip) → full view.
  // Delegated once: history, staged, and future messages all get it for free.
  for (const area of [dom.messages, dom.staged]) {
    area.addEventListener("click", (event) => {
      const thumb = event.target instanceof Element ? event.target.closest(".attach-thumb") : null;
      if (thumb instanceof HTMLImageElement) openLightbox(thumb.src, thumb.alt);
    });
  }

  // The drawer's three doors: the 进度 button toggles the overview, the sidebar's
  // capability entries open their own panel (and switch, not stack).
  dom.progress.addEventListener("click", () => {
    if (dom.drawer.hidden || currentTab !== "overview") switchTab("overview");
    else closeDrawer();
  });
  for (const [button, tab] of [
    [dom.capSkills, "skills"],
    [dom.capMcp, "mcp"],
    [dom.capSchedules, "schedule"],
  ] as const) {
    button.addEventListener("click", () => switchTab(tab));
  }
  for (const [button, tab] of [
    [dom.tabOverview, "overview"],
    [dom.tabSkills, "skills"],
    [dom.tabMcp, "mcp"],
    [dom.tabMemory, "memory"],
  ] as const) {
    button.addEventListener("click", () => switchTab(tab));
  }

  for (const [button, seg] of [
    [dom.memSegLedger, "ledger"],
    [dom.memSegWiki, "wiki"],
    [dom.memSegContext, "context"],
  ] as const) {
    button.addEventListener("click", () => {
      memSeg = seg;
      memNote = null;
      void loadMemory();
    });
  }

  dom.composer.addEventListener("submit", (event) => {
    event.preventDefault();
    void submit();
  });

  dom.input.addEventListener("keydown", (event) => {
    // Enter sends, Shift+Enter adds a newline — the old chat-box convention.
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void submit();
    }
  });

  // Auto-grow: the box follows the draft up to the cap, back to one line when emptied.
  dom.input.addEventListener("input", () => {
    dom.input.style.height = "auto";
    dom.input.style.height = `${Math.min(dom.input.scrollHeight, 180)}px`;
  });

  // 「跑完再说」：不是另一个输入框，而是这一个发送按钮的另一种意思。
  dom.queueToggle.addEventListener("click", () => {
    queuedMode = !queuedMode;
    paintComposerMode();
    dom.input.focus();
  });

  setInterval(() => {
    // Don't refresh while streaming: panel numbers would jump, but the bubble's words matter more.
    if (!busy) {
      void refresh();
      void syncTurns();
    }
  }, POLL_MS);

  // Cross-device pulse: a ping from /api/events (another device turned, an approval is
  // waiting, memory consolidated) → same refresh as the poll, but *now*. open() guards
  // itself with `busy`, so a same-session ping never clobbers a streaming bubble.
  startSync({
    onActivity: (ping) => {
      if (busy) return;
      // Crash recovery announced itself: one quiet line, self-dismissed — the resumed
      // turn has no live stream here (open() pulls history, it does not subscribe), so
      // the statusline must not linger waiting for a TurnCompleted that never comes.
      if (ping.kind === "task_resumed") {
        dom.status.hidden = false;
        dom.status.className = "statusline resumed";
        dom.status.replaceChildren(
          el("span", {
            class: "status-label",
            text: "上次中断的回合正在续跑（已完成的工具不重跑）",
          }),
        );
        if (resumeNotice !== null) window.clearTimeout(resumeNotice);
        resumeNotice = window.setTimeout(hideStatus, 5000);
      }
      void refresh();
      if (ping.sessionId === current) void open(current);
    },
  });

  await refresh();
  await open(current);
  // A bookmarked `#overview` opens the progress drawer; without a hash it stays shut.
  openHashPanel();
  // Working directory: a folder chip. Clicking opens the server-side folder browser —
  // no path is ever typed by hand, and the choice survives a reload.
  setWorkspace(workspace);
  dom.cwd.addEventListener("click", () => void cwdPicker());

  // Attachments: staged first, uploaded on send — backing out mid-way costs nothing.
  dom.attach.addEventListener("click", () => dom.file.click());
  dom.file.addEventListener("change", () => {
    for (const file of dom.file.files ?? []) staged.push(file);
    dom.file.value = "";
    renderStaged();
  });

  dom.input.focus();
}

/** Paint one screen of data. */
/** Pause / continue / cancel the current task — human switches, they exist nowhere else. */
async function switchTask(
  task: AgentTaskView,
  action: "suspend" | "resume" | "cancel",
): Promise<void> {
  if (
    action === "cancel" &&
    !window.confirm(`取消任务「${task.title}」？取消后不会自动续跑。`)
  ) {
    return;
  }
  try {
    await taskAction({ action, taskId: task.id });
  } catch (error) {
    // A dead button is worse than a visible error: say what happened, right on the panel.
    dom.overview.prepend(failureOf(error));
    return;
  }
  await refresh();
}

/** One click: hand the unseen mail to a turn. Errors land on the panel, never silently. */
async function handleInbox(): Promise<void> {
  try {
    await summarizeInbox();
  } catch (error) {
    dom.overview.prepend(failureOf(error));
    return;
  }
  await refresh();
}

async function refresh(): Promise<void> {
  try {
    const data = await fetchDashboard(current);
    joyHome = data.config.home;
    dom.overview.replaceChildren(
      renderOverview(
        data,
        current,
        (id) => void open(id),
        (task, action) => void switchTask(task, action),
        () => void handleInbox(),
      ),
    );
    dom.sessions.replaceChildren(sessionList(data, current, (id) => void open(id)));
    dom.stamp.textContent = formatStamp(data.generatedAt);
  } catch (error) {
    // Failing to fetch data is normal (app-server just started, or the process died),
    // so show it as one thing instead of throwing into a console nobody sees.
    dom.overview.replaceChildren(failureOf(error));
  }
  await refreshCapabilities();
}

/**
 * The capability center's two lists, in the same 15s pulse as everything else.
 *
 * Separate from `/api/data` on purpose: these are files on this machine, not protocol
 * reads, and a failure here must not blank the overview — the sidebar counts simply
 * keep their last value, and an open panel shows the error in place.
 */
async function refreshCapabilities(): Promise<void> {
  const [skills, mcp, schedules] = await Promise.all([
    fetchSkills().catch(() => null),
    fetchMcp().catch(() => null),
    fetchSchedules().catch(() => null),
  ]);
  if (skills !== null) {
    skillsView = skills;
    dom.capSkillsCount.textContent = String(skills.skills.length);
    if (!dom.panelSkills.hidden) paintSkills();
  }
  if (mcp !== null) {
    mcpView = mcp;
    dom.capMcpCount.textContent = String(mcp.servers.length);
    if (!dom.panelMcp.hidden) paintMcp();
  }
  if (schedules !== null) {
    schedulesView = schedules;
    dom.capSchedulesCount.textContent = String(schedules.jobs.length);
    if (!dom.panelSchedule.hidden) paintSchedules();
  }
}

/**
 * Folder picker: the server lists directories, you drill down in the overlay,
 * and "Pick here" fills the absolute path into the top bar's input. Esc or cancel just leaves.
 */
async function cwdPicker(): Promise<void> {
  const overlay = el("div", { class: "fsoverlay" });
  const panel = el("div", { class: "fspanel", role: "dialog", "aria-label": "选择工作目录" });
  overlay.append(panel);
  const close = () => overlay.remove();
  overlay.addEventListener("click", (event) => {
    if (event.target === overlay) close();
  });
  const esc = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      close();
      document.removeEventListener("keydown", esc);
    }
  };
  document.addEventListener("keydown", esc);

  let current = workspace ?? "";
  // Dot-prefixed directories are hidden by default; a fixed toggle sits at the bottom.
  let showHidden = false;
  // "Recents": fetched once when the overlay opens (refreshed after picking).
  let recents = await fetchRecents().catch(() => [] as string[]);

  /** Every hop of the ancestor chain is clickable. Paths not starting with "/" degrade to text. */
  function crumbs(path: string): HTMLElement {
    const bar = el("div", { class: "fscrumbs" });
    if (!path.startsWith("/")) {
      bar.append(el("code", { class: "fscur", text: path }));
      return bar;
    }
    const parts = path.split("/").filter((segment) => segment !== "");
    const root = el("button", { class: "fsc", text: "/", type: "button" });
    root.addEventListener("click", () => void show("/"));
    bar.append(root);
    let prefix = "";
    for (const segment of parts) {
      prefix += `/${segment}`;
      const crumb = el("button", { class: "fsc", text: segment, type: "button" });
      crumb.addEventListener("click", () => void show(prefix));
      bar.append(crumb);
    }
    return bar;
  }

  async function show(path: string | null): Promise<void> {
    panel.replaceChildren(el("p", { class: "hint", text: "读目录中…" }));
    let listing: Awaited<ReturnType<typeof fetchFs>>;
    try {
      listing = await fetchFs(path, showHidden);
    } catch (error: unknown) {
      panel.replaceChildren(
        el("p", { class: "hint", text: `读不动：${String(error)}` }),
        closeButton(),
      );
      return;
    }
    current = listing.path;

    const list = el("div", { class: "fslist" });
    // "Recents" block: frequently visited directories need no digging — one click reaches.
    const here = listing.entries.map((entry) => entry.path);
    const shortcuts = recents.filter((path) => path !== listing.path && !here.includes(path));
    if (shortcuts.length > 0) {
      const block = el("div", { class: "fsrecent" });
      block.append(el("p", { class: "hint", text: "最近使用" }));
      for (const path of shortcuts) {
        const row = el("button", {
          class: "fsrow",
          text: `🕒 ${path}`,
          type: "button",
        });
        row.addEventListener("click", () => void show(path));
        block.append(row);
      }
      list.append(block);
    }
    const rows = listing.entries.map((entry) => {
      const row = el("button", {
        class: "fsrow",
        text: `📁 ${entry.name}`,
        type: "button",
      });
      row.addEventListener("click", () => void show(entry.path));
      return row;
    });
    if (rows.length === 0 && shortcuts.length === 0) {
      list.append(muted("这层没有子目录了。要选的就是当前显示的目录。"));
    } else {
      list.append(...rows);
    }

    // Bottom action bar: new folder + hidden toggle + cancel.
    const mkdir = el("button", { class: "ghost", text: "＋ 新建文件夹", type: "button" });
    mkdir.addEventListener("click", () => {
      const name = window.prompt(`在 ${current} 下新建文件夹：`);
      if (name === null || name.trim() === "") return;
      void createFolder(current, name.trim())
        .then(() => show(current))
        .catch((error: unknown) => window.alert(`建不了：${String(error)}`));
    });
    const hiddenToggle = el("button", {
      class: "ghost",
      text: showHidden ? "隐藏点开头" : "显示隐藏目录",
      type: "button",
    });
    hiddenToggle.addEventListener("click", () => {
      showHidden = !showHidden;
      void show(current);
    });

    const pick = el("button", { class: "approve", text: "选这个目录", type: "button" });
    pick.addEventListener("click", () => {
      setWorkspace(listing.path);
      // Onto the session as well: the folder belongs to this conversation, so it must survive
      // a reload or another device opening the same conversation.
      void updateSession({ sessionId: current, cwd: listing.path }).catch((error: unknown) => {
        console.warn("工作目录没记到会话上（这一轮仍然用它）", error);
      });
      // Recorded into "Recents" — right at the top next time the overlay opens.
      void recordRecent(listing.path).then(() => {
        recents = [listing.path, ...recents.filter((p) => p !== listing.path)].slice(0, 8);
      });
      close();
      document.removeEventListener("keydown", esc);
    });
    panel.replaceChildren(
      crumbs(listing.path),
      list,
      el("div", { class: "row" }, pick, closeButton()),
      el("div", { class: "row" }, mkdir, hiddenToggle),
    );
  }

  function closeButton(): HTMLButtonElement {
    const cancel = el("button", { class: "ghost", text: "取消", type: "button" });
    cancel.addEventListener("click", () => {
      close();
      document.removeEventListener("keydown", esc);
    });
    return cancel;
  }

  document.body.append(overlay);
  await show(current === "" ? null : current);
}

/**
 * Run one capability write, then repaint and leave the outcome on the panel.
 *
 * The server's answer is a sentence a person can read (it refuses built-in rows with a
 * reason), so it is shown as-is — a silent success after a delete is exactly the kind
 * of "did that work?" this page avoids. Returns whether it succeeded, so a form knows
 * whether to close itself.
 */
async function capabilityWrite(
  action: () => Promise<Outcome>,
  panel: HTMLElement,
): Promise<boolean> {
  try {
    const outcome = await action();
    lastOutcome.set(panel, { ok: true, message: outcome.message });
    await refreshCapabilities();
    return true;
  } catch (error) {
    lastOutcome.set(panel, { ok: false, error });
    await refreshCapabilities();
    return false;
  }
}

/// Ask before deleting: a skill is instructions the next turn would have followed.
/// The server refuses built-in names as well — this ask is the second fence, not the only one.
function removeSkillAsk(skill: SkillRow): void {
  const ok = window.confirm(
    `删除技能「${skill.name}」？SKILL.md 会从 ${skill.folder} 移掉，下一轮起不再载入。`,
  );
  if (!ok) return;
  void capabilityWrite(() => removeSkill(skill.name), dom.skillsBody);
}

/// Same for a connector: it stops connecting on the next start.
function removeMcpAsk(server: McpRow): void {
  const ok = window.confirm(`删除 MCP 连接器「${server.name}」？重启 Joy 后不再连接它。`);
  if (!ok) return;
  void capabilityWrite(() => removeMcpServer(server.name), dom.mcpBody);
}

/**
 * Mount `panel` as a modal overlay: backdrop click and Esc both close it.
 * Returns the close function (idempotent — removing twice is a no-op).
 */
function openOverlay(panel: HTMLElement): () => void {
  const overlay = el("div", { class: "fsoverlay" });
  overlay.append(panel);
  document.body.append(overlay);
  const close = (): void => {
    overlay.remove();
    document.removeEventListener("keydown", onKey);
  };
  const onKey = (event: KeyboardEvent): void => {
    if (event.key === "Escape") close();
  };
  overlay.addEventListener("click", (event) => {
    if (event.target === overlay) close();
  });
  document.addEventListener("keydown", onKey);
  return close;
}

/** A minimal SKILL.md, shown as the paste box's placeholder — the format at a glance. */
const SKILL_TEMPLATE = `---
name: my-skill
description: 什么时候用我 —— 触发词写在这里（英文，它会兼任触发器）
---

# 标题

第一步……
第二步……`;

/**
 * Add a skill: paste a SKILL.md, or give a URL (GitHub blob / Gist pages are rewritten
 * to raw content server-side). Validation is the server's — the same gate
 * `joy skill install` passes — so a refusal comes back as a sentence on the panel.
 */
async function skillAddOverlay(): Promise<void> {
  const panel = el("div", {
    class: "fspanel cap-form",
    role: "dialog",
    "aria-label": "添加技能",
  });
  const text = el("textarea", {
    class: "cap-textarea",
    rows: 10,
    placeholder: SKILL_TEMPLATE,
    "aria-label": "SKILL.md 内容",
  });
  const url = el("input", {
    type: "url",
    placeholder: "https://…/SKILL.md（和粘贴框二选一）",
    "aria-label": "SKILL.md 网址",
  });
  const close = openOverlay(panel);

  const install = el("button", { class: "approve", text: "安装", type: "button" });
  install.addEventListener("click", () => {
    install.disabled = true;
    void capabilityWrite(
      () => installSkill({ text: text.value, url: url.value.trim() }),
      dom.skillsBody,
    ).then((ok) => {
      // A refusal keeps the form open: the sentence says what to fix.
      if (ok) close();
      else install.disabled = false;
    });
  });
  const cancel = el("button", { class: "ghost", text: "取消", type: "button" });
  cancel.addEventListener("click", close);

  panel.append(
    el("p", {
      class: "hint",
      text: "粘贴一份 SKILL.md，或填一个网址。名字必须是小写 slug（字母、数字、连字符）—— 它会变成目录名；重名拒绝安装。",
    }),
    text,
    url,
    el("div", { class: "row" }, install, cancel),
  );
  text.focus();
}

/**
 * Add an MCP connector: one name plus **exactly one** transport (a remote url or a local
 * command — the server refuses both). `auth_env` holds the name of an environment
 * variable, never the key itself: mcp.json gets pasted into bug reports.
 */
async function mcpAddOverlay(): Promise<void> {
  const panel = el("div", {
    class: "fspanel cap-form",
    role: "dialog",
    "aria-label": "添加 MCP 连接器",
  });
  const close = openOverlay(panel);

  const name = el("input", { placeholder: "名字，如 fs / notes", "aria-label": "名称" });
  const url = el("input", {
    type: "url",
    placeholder: "https://host/mcp（远程 HTTP 服务器）",
    "aria-label": "远程 URL",
  });
  const command = el("input", {
    placeholder: "npx（本地 stdio 服务器）",
    "aria-label": "本地命令",
  });
  const args = el("input", {
    placeholder: "参数，空格分隔，如 -y @modelcontextprotocol/server-filesystem /tmp",
    "aria-label": "命令参数",
  });
  const authEnv = el("input", {
    placeholder: "鉴权环境变量名（可留空），如 NOTES_API_KEY",
    "aria-label": "鉴权环境变量名",
  });

  const save = el("button", { class: "approve", text: "保存", type: "button" });
  save.addEventListener("click", () => {
    const spec: McpServerSpec = { name: name.value.trim() };
    if (url.value.trim() !== "") {
      spec.url = url.value.trim();
    } else {
      spec.command = command.value.trim();
      const parts = args.value.trim().split(/\s+/).filter((part) => part !== "");
      if (parts.length > 0) spec.args = parts;
    }
    if (authEnv.value.trim() !== "") spec.authEnv = authEnv.value.trim();
    save.disabled = true;
    void capabilityWrite(() => saveMcpServer(spec), dom.mcpBody).then((ok) => {
      // A refusal keeps the form open: the sentence says what to fix.
      if (ok) close();
      else save.disabled = false;
    });
  });
  const cancel = el("button", { class: "ghost", text: "取消", type: "button" });
  cancel.addEventListener("click", close);

  panel.append(
    el("p", {
      class: "hint",
      text: "url 和 command 只能填一个。参数用空格分隔（路径里有空格就先编辑 mcp.json）。鉴权填环境变量的名字，不是密钥本身。保存后重启 Joy 生效 —— 连接发生在启动时。",
    }),
    el("label", { class: "cap-field" }, el("span", { text: "名称" }), name),
    el("label", { class: "cap-field" }, el("span", { text: "远程 URL" }), url),
    el("label", { class: "cap-field" }, el("span", { text: "本地命令" }), command),
    el("label", { class: "cap-field" }, el("span", { text: "命令参数" }), args),
    el("label", { class: "cap-field" }, el("span", { text: "鉴权环境变量名" }), authEnv),
    el("div", { class: "row" }, save, cancel),
  );
  name.focus();
}

/**
 * Add a scheduled job: when it fires (cron) and what it does (one prompt).
 *
 * This form writes `<home>/schedules.json` and nothing else — it does not start a scheduler.
 * The panel's closing note says that out loud; a form that quietly needed a resident process
 * would be the kind of UI that looks finished and does nothing.
 */
async function scheduleAddOverlay(): Promise<void> {
  const panel = el("div", {
    class: "fspanel cap-form",
    role: "dialog",
    "aria-label": "添加定时任务",
  });
  const close = openOverlay(panel);

  const name = el("input", {
    placeholder: "morning-news（小写字母、数字、连字符）",
    "aria-label": "名称",
  });
  const cron = el("input", {
    class: "sched-cron",
    placeholder: "0 8 * * 1-5",
    "aria-label": "cron 表达式",
  });
  const prompt = el("textarea", {
    class: "cap-textarea",
    rows: 4,
    placeholder: "到点让 Joy 做的事，比如：给我一份今天的科技简报，中文，三条以内",
    "aria-label": "触发时做什么",
  });
  const delivery = el(
    "select",
    { "aria-label": "投递方式" },
    el("option", { value: "outbox", text: "outbox —— 写进 <home>/outbox/" }),
    el("option", { value: "notify", text: "notify —— 写文件，同时推送通知" }),
    el("option", { value: "quiet", text: "quiet —— 只记日志（高频任务）" }),
  );

  const save = el("button", { class: "approve", text: "保存", type: "button" });
  save.addEventListener("click", () => {
    const spec: ScheduleSpec = {
      name: name.value.trim(),
      cron: cron.value.trim(),
      prompt: prompt.value.trim(),
      delivery: delivery.value,
    };
    save.disabled = true;
    void capabilityWrite(() => saveSchedule(spec), dom.schedulesBody).then((ok) => {
      // A refusal keeps the form open: the sentence says what to fix.
      if (ok) close();
      else save.disabled = false;
    });
  });
  const cancel = el("button", { class: "ghost", text: "取消", type: "button" });
  cancel.addEventListener("click", close);

  panel.append(
    el("p", {
      class: "hint",
      text: "cron 五段：分 时 日 月 周。`0 8 * * 1-5` 是工作日八点，`30 19 * * *` 是每天十九点半。触发时会在自己的会话里跑一轮。",
    }),
    el("label", { class: "cap-field" }, el("span", { text: "名称" }), name),
    el("label", { class: "cap-field" }, el("span", { text: "时间" }), cron),
    el("label", { class: "cap-field" }, el("span", { text: "做什么" }), prompt),
    el("label", { class: "cap-field" }, el("span", { text: "投递" }), delivery),
    el("div", { class: "row" }, save, cancel),
  );
  name.focus();
}

/// Deleting a job from the file: the row disappears from the list at once, and the scheduler
/// stops seeing it on its next tick. Result files already written are left alone.
function removeScheduleAsk(job: ScheduleJob): void {
  const ok = window.confirm(
    `删除定时任务「${job.name}」？schedules.json 里那一行会被移掉，joy schedule 下次触发前就读不到它。已经写出的 outbox 文件不动。`,
  );
  if (!ok) return;
  void capabilityWrite(() => removeSchedule(job.name), dom.schedulesBody);
}

async function submit(): Promise<void> {
  const text = dom.input.value.trim();
  if (text === "" && staged.length === 0) return;

  // While a turn is running, "send" means **steer** (0.16): the sentence joins the round in
  // flight instead of starting a second one (which the server refuses with SESSION_BUSY).
  // Attachments cannot ride it — that request was already built — so they wait, out loud.
  if (busy || otherTurn !== null) {
    if (staged.length > 0) {
      showStatus("thinking", "这一轮还在跑 —— 附件留到下一轮");
      return;
    }
    if (text === "") return;
    // Two intents, one button: "and also, while you are at it" versus "when you are done".
    if (queuedMode) await followUp(text);
    else await steer(text);
    return;
  }

  // Upload attachments first. Images ride as real attachments (the turn embeds the bytes —
  // the model sees pixels); other files keep the "path in text" route (the model opens them
  // with read_file). Upload order is preserved, so message text maps one-to-one to files.
  let full = text;
  let attachments: TurnAttachment[] | null = null;
  if (staged.length > 0) {
    const home = joyHome;
    const lines: string[] = [];
    try {
      for (const file of staged) {
        const path = await uploadFile(file);
        if (file.type.startsWith("image/")) {
          if (file.size > 20 * 1024 * 1024) {
            dom.staged.replaceChildren(failureOf(new Error(`${file.name} 超过 20MB 单图上限`)));
            return;
          }
          attachments ??= [];
          attachments.push({ path, mediaType: file.type });
        } else {
          lines.push(`（附件：${home}/${path}）`);
        }
      }
    } catch (error: unknown) {
      // An upload failure is not the end of the world: staging stays, so one can retry or drop.
      dom.staged.replaceChildren(failureOf(error));
      return;
    }
    if (lines.length > 0) {
      full = `${lines.join("\n")}${text === "" ? "" : `\n${text}`}`;
    }
  }

  // Snapshot the staged files for the message's attachment strip BEFORE clearing the stage —
  // the object URLs stay alive so the thumbnails in the flow keep resolving.
  const snapshot = staged.slice();
  dom.input.value = "";
  dom.input.style.height = "auto";
  staged.length = 0;
  renderStaged();
  await send(full, attachments, snapshot);
}

/**
 * Add a sentence to the turn that is already running (0.16).
 *
 * The bubble is painted right away because the message **is** accepted — it lands in the
 * transcript either way. Whether the model saw it before the turn ended is not something this
 * page can promise, so it does not claim it: "下一轮次生效" is the honest wording.
 */
async function steer(text: string): Promise<void> {
  dom.input.value = "";
  dom.input.style.height = "auto";
  dom.messages.append(el("div", { class: "msg me" }, el("div", { class: "bubble", text })));
  scrollToEnd(true);

  try {
    const { queued } = await steerTurn({ sessionId: current, message: text });
    // `queued` counts everything this turn accepted: 1 = it will be seen on the next model
    // call, more = it queues behind the earlier ones.
    showStatus("thinking", queued > 1 ? `已补充（本轮第 ${queued} 句，排队）` : "已补充，下一轮次生效");
  } catch (error: unknown) {
    dom.messages.append(failureOf(error));
    scrollToEnd(true);
  }
}

/**
 * Queue this sentence to run **after** the turn in flight (0.16).
 *
 * The person's wording is what makes this method exist: `steer` means "and also, while you are
 * at it", this means "when you are done". So it becomes its own turn with its own rounds and
 * budget, and it runs even if this page closes — the queue outlives the browser.
 *
 * The bubble is painted now because the sentence **is** accepted now; the statusline carries
 * the part that differs: it runs later, behind whatever is already waiting.
 */
async function followUp(text: string): Promise<void> {
  dom.input.value = "";
  dom.input.style.height = "auto";
  dom.messages.append(el("div", { class: "msg me" }, el("div", { class: "bubble", text })));
  scrollToEnd(true);

  try {
    // `queued` is everything waiting in this session, this sentence included.
    const { queued } = await followUpTurn({ sessionId: current, message: text });
    showStatus(
      "thinking",
      queued > 1 ? `已排队 —— 前面还有 ${queued - 1} 条` : "已排队 —— 这一轮跑完就轮到它",
    );
  } catch (error: unknown) {
    // The usual cause is the turn ending between the click and the request; the probe below
    // brings `otherTurn` back in line so the next send is not addressed to a finished turn.
    dom.messages.append(failureOf(error));
    scrollToEnd(true);
    void syncTurns();
  }
}

/** Say one thing, painting the course of this turn. */
async function send(
  text: string,
  attachments: TurnAttachment[] | null = null,
  previewFiles: File[] = [],
): Promise<void> {
  setBusy(true);

  // The welcome hero only lives until the first sentence arrives.
  dom.messages.querySelector(".welcome")?.remove();

  const mine = el("div", { class: "msg me" }, el("div", { class: "bubble", text }));
  if (previewFiles.length > 0) {
    mine.append(
      attachmentStrip(previewFiles.map((file) => ({ name: file.name, url: previewUrl(file) }))),
    );
  }
  dom.messages.append(mine);

  // The turn's course lives on a timeline (collapsed to its latest step); the trace
  // row keeps only what needs interaction: approval cards and hard failures.
  const timeline = createTimeline();
  const trace = el("div", { class: "trace" });
  // md from the first byte: the stream paints real Markdown as it arrives
  // (StreamMarkdown, below) — not raw `**` that flips into rendered text at the end.
  const bubble = el("div", { class: "bubble md streaming" });
  const stream = new StreamMarkdown(bubble);
  const foot = el("div", { class: "foot" });
  dom.messages.append(
    el("div", { class: "msg joy" }, timeline.root, trace, bubble, foot),
  );
  scrollToEnd(true);

  let streamed = "";
  let completed: TurnCompletedNotification | null = null;
  let firstDelta = true;
  showStatus("thinking", "思考中");

  try {
    // The picked folder travels with every turn: it is the sandbox root for writes.
    for await (const notification of talk({
      sessionId: current,
      message: text,
      stream: true,
      cwd: workspace,
      attachments,
    })) {
      switch (notification.type) {
        case "turnStarted":
          // The turn's handle arrives first — from here on, stopping is possible.
          activeTurnId = notification.turnId;
          addStopButton();
          break;

        case "textDelta":
          if (firstDelta) {
            firstDelta = false;
            showStatus("thinking", "回复中");
          }
          streamed += notification.delta;
          // Incremental paint, but at **block** granularity: settled blocks keep their
          // nodes, only the still-growing last block is repainted — once per animation
          // frame, not once per delta. A several-thousand-token reply must not reflow
          // on every chunk, yet Markdown shows up while it streams, not after.
          stream.push(notification.delta);
          scrollToEnd(true);
          break;

        case "gateDecided":
          // The gate decides whether to consult memory. The line shows "why it remembered / not".
          timeline.gate(
            notification.decision.decision === "retrieve"
              ? `检索「${notification.decision.query ?? ""}」`
              : `跳过记忆（${notification.decision.reason}）`,
          );
          break;

        case "approvalRequested": {
          // An authorization card inline in the flow: nothing moves until clicked — deny by default.
          const card = el("div", { class: "approval-card" });
          card.append(
            el(
              "div",
              { class: "ac-head" },
              el("span", { class: "ac-dot", "aria-hidden": "true" }),
              el("span", { class: "ac-title", text: "需要你授权" }),
              el("span", { class: "tag", text: notification.tool }),
            ),
            el("code", { class: "ac-cmd", text: notification.argsPreview }),
          );
          const foot = el("div", { class: "ac-foot" });
          card.append(foot);
          const settle = (approved: boolean, remember: boolean) => {
            card.querySelectorAll("button").forEach((button) => {
              (button as HTMLButtonElement).disabled = true;
            });
            void respondApproval({
              turnId: notification.turnId,
              requestId: notification.requestId,
              approved,
              remember,
            })
              .then((response) => {
                // The verdict replaces the buttons — the card becomes its own receipt.
                foot.replaceChildren(
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
                foot.replaceChildren(chip(`回答没送出去：${String(error)}`, "note"));
              });
          };
          const allow = el("button", { class: "approve", text: "允许" });
          const always = el("button", { class: "approve", text: "允许并记住" });
          const deny = el("button", { class: "deny", text: "拒绝" });
          allow.addEventListener("click", () => settle(true, false));
          always.addEventListener("click", () => settle(true, true));
          deny.addEventListener("click", () => settle(false, false));
          foot.append(allow, always, deny);
          trace.append(card);
          break;
        }

        case "goalRound":
          // The goal loop ran another round: say "why it hasn't ended yet", otherwise the UI
          // moved a round on its own and nobody knows why.
          timeline.goal(
            `目标 · 第 ${notification.round}/${notification.maxRounds} 轮 · ${notification.status} — ${notification.reason}`,
          );
          break;

        case "retry":
          // Retries are never silent: this line shows "those seconds weren't a hang".
          timeline.retry(
            `重试第 ${notification.attempt} 次（${notification.reason}，${formatMs(
              notification.delayMs,
            )} 后）`,
          );
          break;

        case "toolStarted": {
          // Plain-language summary on the thread; the JSON args stay one hover away.
          showStatus("working", notification.tool);
          timeline.toolStart(notification.tool, toolSummary(notification));
          scrollToEnd(true);
          break;
        }

        case "toolCompleted": {
          // The end freezes the entry: status coloring + duration.
          timeline.toolEnd(
            notification.tool,
            notification.status === "ok",
            notification.durationMs,
            notification.output,
          );
          scrollToEnd(true);
          break;
        }

        case "consolidationCompleted":
          if (notification.newFacts > 0) {
            timeline.note(`记住了 ${notification.newFacts} 条新事实`);
            void refresh(); // The memory panel catches up now, no need to wait for the next poll
          }
          break;

        case "graphStarted":
          timeline.graph(`工作流 ${notification.workflow}`);
          break;

        case "graphNodeStarted":
          timeline.graph(`${notification.node} #${notification.visit}`);
          break;

        case "graphNodeEnded":
          timeline.graph(`${notification.node} · ${formatMs(notification.ms)}`);
          break;

        case "graphEnded":
          timeline.graph(`工作流 ${notification.workflow} · ${notification.path.length} 步`);
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
    // Wrap-up lives in finally: even a mid-way error must clear the "writing" state,
    // otherwise the blinking cursor would stay on the bubble forever.
    bubble.classList.remove("streaming");
    hideStatus();
    timeline.settle(); // fold back: after the turn the thread collapses to one line — the reply gets the attention

    if (completed !== null) {
      // Overwrite the streamed accumulation with `reply`: deltas may be missing (backpressure,
      // disconnection), while reply is the authoritative result from app-server.
      paintReply(bubble, completed.reply === "" ? "（这一轮没有回复）" : completed.reply);
      foot.textContent = turnFooter(completed);
    } else if (streamed !== "") {
      // Truncated mid-way: show what did arrive — the stream already painted it as
      // Markdown, finish() just completes the still-growing last block.
      stream.finish();
      foot.textContent = "这一轮没有收到结尾 —— 上面的回复可能被截断了";
    } else {
      bubble.textContent = "（这一轮没有回复）";
    }

    scrollToEnd(true);
    setBusy(false);
    // This turn changed the session's message count and last-active time; refresh the panels.
    void refresh();
    trimMessages();
    dom.input.focus();
  }
}

/**
 * Swap the streamed paint for the authoritative reply, exactly once.
 *
 * The stream already renders Markdown live (StreamMarkdown); this full repaint exists
 * only because `reply` from app-server is the truth — deltas may be missing
 * (backpressure, disconnection) or differ from what actually landed in the transcript.
 */
function paintReply(bubble: HTMLElement, markdown: string): void {
  bubble.classList.add("md");
  bubble.replaceChildren(renderMarkdown(markdown));
}

/**
 * Switch to a session: change the name, pull back the history, get ready to talk there.
 *
 * No switching while streaming — that turn's bytes would otherwise pour into another session.
 */
/**
 * Start a new line. A session is only a label on chat_log — there is nothing to create on the
 * server; the first turn writes it into existence. So "new" means: pick an unused id, clear
 * every piece of the previous session's state (draft, attachments, status line) and
 * paint the empty welcome screen. The old session stays in the list, one click away.
 */
async function newSession(): Promise<void> {
  if (busy) return;
  // The server allocates the id (0.15): two tabs generating ids here is how conversations end
  // up sharing a row. A failure falls back to a local id rather than blocking the composer —
  // the first message creates the row anyway, the only cost is that it has no name yet.
  const id = await createSession({ cwd: workspace }).then(
    (response) => response.sessionId,
    (error: unknown) => {
      console.warn("会话没建到服务端（首条消息会补上）", error);
      return `s-${Date.now().toString(36)}-${crypto.randomUUID().slice(0, 4)}`;
    },
  );
  current = id;
  olderCursor = null;
  activeTurnId = null;
  dom.session.textContent = id;
  dom.input.value = "";
  dom.input.style.height = "auto";
  staged.length = 0;
  renderStaged();
  dom.messages.replaceChildren(welcome());
  dom.older.hidden = true;
  queuedMode = false;
  setOtherTurn(null);
  hideStatus();
  // Refresh paints the session list with a "新会话" placeholder for this id (see render.ts).
  await refresh();
  dom.input.focus();
  scrollToEnd();
}

async function open(sessionId: string): Promise<void> {
  if (busy) return;

  current = sessionId;
  queuedMode = false; // a new conversation: the previous one's 「跑完再说」 is not its own
  dom.session.textContent = sessionId;
  dom.older.hidden = true;

  // The working directory travels with the session (0.15), not with the browser: switching
  // conversations must switch the folder too, or the next write lands in the previous project.
  // `null` on the server means this conversation never chose one — clearing the picker is the
  // honest rendering, not "keep whatever the last session had".
  void fetchSessionMeta(sessionId)
    .then((meta) => {
      if (current !== sessionId) return; // switched again while this was in flight
      setWorkspace(meta.session.cwd ?? null);
    })
    .catch((error: unknown) => {
      console.warn("会话的元数据没读到（工作目录沿用当前）", error);
    });

  try {
    await loadHistory(sessionId, null);
  } catch (error) {
    dom.messages.replaceChildren(failureOf(error));
    return;
  }
  scrollToEnd();
  // A turn may already be running here (another device, a queued follow-up): the reloaded
  // page has no way to know from its own state, so it asks.
  void syncTurns();

  // The task card belongs to the session too, not to the history page: without this, a switch
  // keeps showing the previous session's card until the next 15s poll — and "the task that
  // just finished" is often exactly why the person switched over.
  void refresh();
}

/**
 * Fetch one page of history and paint it.
 *
 * Empty `cursor` = switching sessions or first open, replace the whole screen; given = paging
 * back, prepended at the front. This is how `session/messages` works: the protocol gives
 * **newest first** (easy to page toward earlier), the screen needs it upright, so flip here.
 */
async function loadHistory(sessionId: string, cursor: string | null): Promise<void> {
  const page = await fetchMessages(sessionId, cursor);
  const nodes = [...page.data].reverse().map(messageRow);

  if (cursor === null) {
    dom.messages.replaceChildren(
      ...(nodes.length === 0 ? [welcome()] : nodes),
    );
    trimMessages();
  } else {
    // Prepend at the top and **keep the original scroll position** — otherwise paging flings it.
    const fromBottom = dom.messages.scrollHeight - dom.messages.scrollTop;
    dom.messages.prepend(...nodes);
    dom.messages.scrollTop = dom.messages.scrollHeight - fromBottom;
  }

  olderCursor = page.nextCursor;
  dom.older.hidden = olderCursor === null;
}

/** Page back one screen: older messages. */
async function older(): Promise<void> {
  if (busy || olderCursor === null) return;

  dom.older.disabled = true;
  try {
    await loadHistory(current, olderCursor);
  } catch (error) {
    // On failure, leave this line at the top; don't let "I clicked and nothing happened" happen.
    dom.messages.prepend(failureOf(error));
  } finally {
    dom.older.disabled = false;
  }
}

/**
 * Ask `turn/list`: is anything running on this session right now?
 *
 * Skipped while this page streams its own turn — `busy` already answers, and a probe would
 * race with that turn's start and end. A failed probe keeps the previous answer: app-server
 * may be restarting, and flickering between "running" and "nothing" is worse than going
 * quiet for one poll.
 */
async function syncTurns(): Promise<void> {
  if (busy) return;
  const sessionId = current;
  try {
    const { turns } = await fetchTurns(sessionId);
    if (current !== sessionId) return; // switched while the probe was in flight
    setOtherTurn(turns[0] ?? null);
  } catch {
    // Next poll answers; silence beats a wrong claim about someone else's turn.
  }
}

/**
 * Record that a turn runs elsewhere, and say so on the statusline — the one place this page
 * already talks about the current turn. Re-asserted on every poll so the elapsed time stays
 * current (and so a steer confirmation, which paints the same line, cannot outlive it).
 *
 * Stopping it from here is legitimate too: `turn/interrupt` works by id alone, so the stop
 * button gets that id — "it is stuck over there" is still the human's problem to solve.
 */
function setOtherTurn(turn: TurnView | null): void {
  const wasRunning = otherTurn !== null;
  otherTurn = turn?.startedAt ?? null;
  dom.composer.classList.toggle("elsewhere", turn !== null);
  paintComposerMode();

  if (turn === null) {
    if (wasRunning && !busy) hideStatus();
    return;
  }
  if (busy) return;
  const since = Date.parse(turn.startedAt);
  showStatus(
    "working",
    Number.isNaN(since)
      ? "这一轮在别处跑 —— 可以补充它，也可以跑完再说"
      : `这一轮在别处跑了 ${elapsedSince(since)} —— 可以补充它，也可以跑完再说`,
  );
  activeTurnId = turn.turnId;
  addStopButton();
}

/**
 * Say what pressing send will actually do right now.
 *
 * One button, three meanings — a fresh turn (idle), adding to the turn in flight (steer), or
 * queueing behind it (follow-up) — so the label changes with the moment instead of leaving the
 * person to guess. The toggle itself only exists while there is a turn to follow.
 */
function paintComposerMode(): void {
  const inFlight = busy || otherTurn !== null;
  const queuedNow = inFlight && queuedMode;
  dom.queueToggle.hidden = !inFlight;
  dom.queueToggle.classList.toggle("on", queuedNow);
  dom.queueToggle.setAttribute("aria-pressed", String(queuedNow));
  dom.send.textContent = !inFlight ? "发送" : queuedNow ? "排队" : "补充";
}

function setBusy(value: boolean): void {
  busy = value;
  // A turn of our own is the answer now: whatever ran elsewhere is superseded by this stream.
  if (value) {
    queuedMode = false; // a mode chosen for the last turn does not carry into this one
    setOtherTurn(null);
  }
  dom.send.disabled = value;
  // Switching sessions mid-stream would pour that turn's bytes into another session.
  dom.newSession.disabled = value;
  dom.composer.classList.toggle("busy", value);
  paintComposerMode();
}

/**
 * Scrolling policy: follow the stream **only while the reader is at the bottom** — someone
 * paging back through history must not be yanked down by every delta. `interactive` marks
 * a moment that always scrolls (the user just sent something).
 */
function nearBottom(): boolean {
  return dom.messages.scrollHeight - dom.messages.scrollTop - dom.messages.clientHeight < 120;
}

function scrollToEnd(interactive = false): void {
  if (interactive || nearBottom()) {
    dom.messages.scrollTop = dom.messages.scrollHeight;
  }
}

/**
 * Cap the live DOM: the store keeps everything, the page keeps a window. Beyond
 * MAX_MESSAGES children the oldest nodes are dropped — paging back is exactly what
 * the「更早的」button is for, so nothing is lost, just not painted.
 */
const MAX_MESSAGES = 300;

function trimMessages(): void {
  const overflow = dom.messages.children.length - MAX_MESSAGES;
  if (overflow <= 0) return;
  for (let i = 0; i < overflow; i++) {
    dom.messages.firstElementChild?.remove();
  }
  if (dom.messages.querySelector(".folded-note") === null) {
    dom.messages.prepend(
      el("p", { class: "hint folded-note", text: "更早的消息已折叠 —— 用上面的「更早的」翻看" }),
    );
  }
}

function must<T extends HTMLElement = HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  // Blow up on the spot when not found. Silently doing nothing is far harder to debug —
  // and the only possible cause is index.html and this getting out of sync.
  if (node === null) {
    stalePage(id);
    throw new Error(`页面里没有 #${id}`);
  }
  return node as T;
}

/**
 * The page and the bundle are not the same build — and that used to mean a blank screen.
 *
 * `index.html` is network-first while `main.js` is cache-first, so a service-worker cache
 * can hand a *new* bundle an *old* page. The `must()` above then threw at module scope,
 * which aborts the whole bundle: no session list, no chat, no error — just an empty page
 * and one console line nobody opens. This is that failure given a face: what happened,
 * and the single click that fixes it (drop the stale caches, refetch).
 *
 * Styling is inline on purpose: style.css may be the stale half too.
 */
function stalePage(id: string): void {
  if (document.body === null) return;
  const reload = el("button", {
    type: "button",
    text: "清掉旧缓存并重新加载",
    style:
      "margin-top:18px;padding:9px 18px;border:1px solid #6b6f78;border-radius:9px;" +
      "background:transparent;color:inherit;cursor:pointer;font:inherit",
  });
  reload.addEventListener("click", () => {
    void (async () => {
      const regs = await navigator.serviceWorker?.getRegistrations?.() ?? [];
      await Promise.all(regs.map((reg) => reg.unregister()));
      if (typeof caches !== "undefined") {
        const keys = await caches.keys();
        await Promise.all(keys.map((key) => caches.delete(key)));
      }
      location.reload();
    })();
  });
  document.body.replaceChildren(
    el(
      "div",
      {
        style:
          "max-width:560px;margin:16vh auto 0;padding:0 24px;color:#c9cbd1;" +
          "font:15px/1.75 -apple-system,system-ui,'Segoe UI',sans-serif",
      },
      el("h1", { text: "页面和脚本不是同一版", style: "font-size:18px;margin:0 0 14px" }),
      el("p", {
        text: `这个页面缺了 #${id} —— 浏览器缓存了旧版脚本（service worker 缓存优先），新页面配上了旧 bundle。`,
        style: "margin:0 0 6px",
      }),
      el("p", {
        text: "点下面的按钮：会注销 service worker、清掉旧缓存，再取一份新版。",
        style: "margin:0;color:#8d919a",
      }),
      reload,
    ),
  );
}

// PWA：静态资源交给 service worker（缓存优先、后台更新）；/api 一帧不缓存。
// 仅 https 或 localhost 下注册——这是浏览器自己的要求，失败不吵（安装能力是锦上添花）。
if (
  "serviceWorker" in navigator &&
  (location.protocol === "https:" ||
    location.hostname === "localhost" ||
    location.hostname === "127.0.0.1")
) {
  navigator.serviceWorker.register("./sw.js").catch(() => undefined);
}

// 登录门：远程模式（配置了服务器地址）且本设备还没有 token → 先登录，再进应用。
// 本机同源模式不设门——`joy dashboard` 打开即用，401 时才有一次性 token 询问。
if (localStorage.getItem("joy-server") !== null && localStorage.getItem("joy-ops-token") === null) {
  showLogin({ onAuthed: () => void boot() });
} else {
  void boot();
}

// 远程模式下的 401：会话过期或 token 被撤。登录卡回来，成功后整页重载拿干净状态。
window.addEventListener("joy:unauthorized", () => {
  showLogin({ notice: "登录已过期，请重新登录", onAuthed: () => location.reload() });
});
