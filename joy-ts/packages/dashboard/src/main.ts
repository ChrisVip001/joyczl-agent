// The dashboard's main program: on boot, pull back the current session's history, then wait for input.
//
// The page itself is **stateless**: sessions, memory, and config all live on the Rust side
// (behind app-server is state.db). The only temporary view state here is "which session is being
// viewed" and "whether a turn is streaming"; a refresh returns to defaults. In other words, if the
// browser crashes or the tab closes, nothing is lost — that's the most valuable part of this layering.

import type { TurnAttachment, TurnCompletedNotification } from "./protocol";
import { createTimeline } from "./timeline";
import { showLogin } from "./login";
import {
  fetchApprovals,
  fetchDashboard,
  fetchMessages,
  fetchModelProfiles,
  fetchSchedules,
  fetchUsage,
  interruptTurn,
  removeModel,
  respondApproval,
  saveModel,
  setGoal,
  createFolder,
  fetchFs,
  fetchRecents,
  recordRecent,
  talk,
  uploadFile,
} from "./api";
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
  renderApprovals,
  renderMemory,
  renderOverview,
  renderSchedules,
  renderUsage,
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
  goal: must<HTMLInputElement>("goal"),
  goalSet: must<HTMLButtonElement>("goal-set"),
  goalClear: must<HTMLButtonElement>("goal-clear"),
  goalState: must<HTMLElement>("goal-state"),
  mpState: must<HTMLElement>("mp-state"),
  mpPicker: must<HTMLSelectElement>("mp-picker"),
  mpId: must<HTMLInputElement>("mp-id"),
  mpProvider: must<HTMLInputElement>("mp-provider"),
  mpModel: must<HTMLInputElement>("mp-model"),
  mpKey: must<HTMLInputElement>("mp-key"),
  mpSave: must<HTMLButtonElement>("mp-save"),
  mpRemove: must<HTMLButtonElement>("mp-remove"),
  send: must<HTMLButtonElement>("send"),
  refresh: must<HTMLButtonElement>("refresh"),
  older: must<HTMLButtonElement>("older"),
  overview: must("overview"),
  memory: must("memory"),
  usage: must("usage"),
  approvals: must("approvals"),
  schedules: must("schedules"),
  status: must("status"),
  model: must<HTMLSelectElement>("model"),
  cwd: must<HTMLInputElement>("cwd"),
  attach: must<HTMLButtonElement>("attach"),
  file: must<HTMLInputElement>("file"),
  staged: must<HTMLElement>("staged"),
  home: must("home"),
  session: must("session"),
  stamp: must("stamp"),
  sessions: must("sessions"),
  drawer: must("drawer"),
  drawerClose: must<HTMLButtonElement>("drawer-close"),
};

let busy = false;

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
 * Navigation for the right-side panels (data as navigation).
 * Overview/memory follow the full-screen poll; usage/approvals/schedules are fetched on demand —
 * only the **currently open** tab is refreshed, paying no requests for panels that aren't visible.
 */
type TabId = "overview" | "memory" | "usage" | "approvals" | "schedules" | "model";
let activeTab: TabId = "overview";

function switchTab(tab: TabId): void {
  activeTab = tab;
  for (const button of document.querySelectorAll<HTMLButtonElement>(".tabs .tab")) {
    button.classList.toggle("active", button.dataset.tab === tab);
  }
  for (const section of document.querySelectorAll<HTMLElement>(".panels .panel")) {
    section.hidden = section.id !== `panel-${tab}`;
  }
  openDrawer();
  void refreshPanels();
}

/** The panel drawer slides in from the right; the chat column never squeezes. */
function openDrawer(): void {
  dom.drawer.hidden = false;
}

function closeDrawer(): void {
  dom.drawer.hidden = true;
}

/** On-demand panels: only fire a request when the current tab is one of the three new panels. */
async function refreshPanels(): Promise<void> {
  try {
    if (activeTab === "usage") {
      dom.usage.replaceChildren(renderUsage(await fetchUsage()));
    } else if (activeTab === "approvals") {
      dom.approvals.replaceChildren(renderApprovals((await fetchApprovals()).entries));
    } else if (activeTab === "schedules") {
      dom.schedules.replaceChildren(renderSchedules((await fetchSchedules()).jobs));
    }
  } catch (error) {
    const target =
      activeTab === "usage"
        ? dom.usage
        : activeTab === "approvals"
          ? dom.approvals
          : activeTab === "schedules"
            ? dom.schedules
            : null;
    if (target !== null) target.replaceChildren(failureOf(error));
  }
}

// --- Run state, three kinds -----------------------
//
// "Waiting for you" is static; "Thinking/Replying" gets shimmer + stopwatch; "Working" carries
// the tool name. It distinguishes "stuck" from "running" — a moving stopwatch means the latter.

let statusTimer: number | null = null;
let statusStart = 0;
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
 * Refresh the model panel: **no keys in the response** — only the active profile and key location.
 */
async function refreshModels(): Promise<void> {
  try {
    const response = await fetchModelProfiles();
    const active = response.profiles.find((profile) => profile.active);
    dom.mpState.textContent = active
      ? `当前：${active.id} · ${active.provider}${
          active.model ? ` / ${active.model}` : ""
        } · key 在 ${active.keySource ?? "—"}`
      : "当前：还没配（环境变量里的那套在用）";

    dom.mpPicker.replaceChildren();
    for (const profile of response.profiles) {
      const option = document.createElement("option");
      option.value = profile.id;
      option.textContent = `${profile.active ? "★ " : ""}${profile.id} · ${profile.provider}`;
      option.selected = profile.active;
      dom.mpPicker.append(option);
    }
    dom.mpPicker.hidden = response.profiles.length === 0;

    // Quick switch in the header (pi's /model shape): one glance away, no panel needed.
    dom.model.replaceChildren();
    if (response.profiles.length === 0) {
      const option = document.createElement("option");
      option.textContent = "没配模型";
      dom.model.append(option);
      dom.model.disabled = true;
    } else {
      dom.model.disabled = false;
      for (const profile of response.profiles) {
        const option = document.createElement("option");
        option.value = profile.id;
        option.textContent = `${profile.id} · ${profile.provider}`;
        option.selected = profile.active;
        dom.model.append(option);
      }
    }
  } catch (error: unknown) {
    dom.mpState.textContent = `模型面板没刷出来：${String(error)}`;
  }
}

/**
 * Save (and switch). Filling the key field with `env:VAR_NAME` only reads the env var
 * without storing it; a real key is handed to the system keychain first.
 */
/**
 * Switch to a configured model profile: only the id is sent, the rest left empty = keep
 * as is (a switch shouldn't re-send the key). Shared by the header dropdown and the panel.
 */
async function switchModel(id: string): Promise<void> {
  if (id === "") return;
  try {
    const response = await saveModel({
      id,
      provider: null,
      model: null,
      smallModel: null,
      baseUrl: null,
      apiKey: null,
      keyEnv: null,
      makeActive: true,
    });
    dom.mpState.textContent = `已切到 ${response.id}（${response.current ?? "还没配好"}）`;
  } catch (error: unknown) {
    dom.mpState.textContent = `切不过去：${String(error)}`;
  }
  await refreshModels();
}

async function applyModel(): Promise<void> {
  const id = dom.mpId.value.trim();
  if (id === "") {
    dom.mpState.textContent = "先给这套起个名字。";
    return;
  }
  const rawKey = dom.mpKey.value.trim();
  const fromEnv = rawKey.startsWith("env:");
  try {
    const response = await saveModel({
      id,
      provider: dom.mpProvider.value.trim() || null,
      model: dom.mpModel.value.trim() || null,
      smallModel: null,
      baseUrl: null,
      apiKey: !fromEnv && rawKey !== "" ? rawKey : null,
      keyEnv: fromEnv ? rawKey.slice(4) : null,
      makeActive: true,
    });
    dom.mpState.textContent = `已切到 ${response.id}（${
      response.current ?? "还没配好"
    }）；密钥存在：${response.keyStoredIn}`;
    dom.mpKey.value = "";
    await refreshModels();
  } catch (error: unknown) {
    dom.mpState.textContent = `没存上：${String(error)}`;
  }
}

async function boot(): Promise<void> {
  dom.refresh.addEventListener("click", () => void refresh());
  dom.older.addEventListener("click", () => void older());
  dom.drawerClose.addEventListener("click", closeDrawer);

  // Click a thumbnail anywhere in the flow (or the staging strip) → full view.
  // Delegated once: history, staged, and future messages all get it for free.
  for (const area of [dom.messages, dom.staged]) {
    area.addEventListener("click", (event) => {
      const thumb = event.target instanceof Element ? event.target.closest(".attach-thumb") : null;
      if (thumb instanceof HTMLImageElement) openLightbox(thumb.src, thumb.alt);
    });
  }

  // Right-side tabs: data as navigation — each button carries one tab name; behavior in switchTab.
  for (const button of document.querySelectorAll<HTMLButtonElement>(".tabs .tab")) {
    button.addEventListener("click", () => {
      if (button.dataset.tab !== undefined) switchTab(button.dataset.tab as TabId);
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

  setInterval(() => {
    // Don't refresh while streaming: panel numbers would jump, but the bubble's words matter more.
    if (!busy) {
      void refresh();
      void refreshPanels();
    }
  }, POLL_MS);

  await refresh();
  await open(current);
  // Goal bar: set/clear both go through the same place (`goal/set`) — same as `/goal` in the terminal.
  const applyGoal = (condition: string | null) => {
    void setGoal({ sessionId: current, condition })
      .then((response) => {
        dom.goalState.textContent = response.active
          ? `进行中（最多 ${response.maxRounds} 轮）`
          : "没有目标";
        if (!response.active) dom.goal.value = "";
      })
      .catch((error: unknown) => {
        dom.goalState.textContent = `没设上：${String(error)}`;
      });
  };
  dom.goalSet.addEventListener("click", () => {
    const condition = dom.goal.value.trim();
    if (condition !== "") applyGoal(condition);
  });
  dom.goalClear.addEventListener("click", () => applyGoal(null));
  dom.goal.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      const condition = dom.goal.value.trim();
      if (condition !== "") applyGoal(condition);
    }
  });

  // The working directory's "Browse" button: pick a folder like a desktop app (server lists it).
  const cwdBrowse = el("button", {
    class: "ghost",
    text: "浏览…",
    title: "选一个文件夹作为工作目录（也可以直接在左边手输）",
    type: "button",
  });
  cwdBrowse.addEventListener("click", () => void cwdPicker());
  dom.cwd.before(cwdBrowse);

  // Attachments: staged first, uploaded on send — backing out mid-way costs nothing.
  dom.attach.addEventListener("click", () => dom.file.click());
  dom.file.addEventListener("change", () => {
    for (const file of dom.file.files ?? []) staged.push(file);
    dom.file.value = "";
    renderStaged();
  });

  // Header quick switch: same implementation as the panel dropdown (saveModel with id only = keep).
  dom.model.addEventListener("change", () => {
    void switchModel(dom.model.value).then(() => refreshModels());
  });

  // Model panel: switch, save, remove all go through `model/*` — same as /model in the terminal.
  dom.mpSave.addEventListener("click", () => void applyModel());
  dom.mpPicker.addEventListener("change", () => {
    void switchModel(dom.mpPicker.value);
  });
  dom.mpRemove.addEventListener("click", () => {
    const id = dom.mpPicker.value || dom.mpId.value.trim();
    if (id === "") return;
    void removeModel({ id })
      .then(async (response) => {
        dom.mpState.textContent = response.removed ? `已删掉 ${id}` : `没有叫 ${id} 的配置`;
        await refreshModels();
      })
      .catch((error: unknown) => {
        dom.mpState.textContent = `删不掉：${String(error)}`;
      });
  });
  void refreshModels();

  dom.input.focus();
}

/** Paint one screen of data. */
async function refresh(): Promise<void> {
  try {
    const data = await fetchDashboard(current);
    dom.overview.replaceChildren(renderOverview(data, current, (id) => void open(id)));
    dom.memory.replaceChildren(renderMemory(data));
    dom.sessions.replaceChildren(sessionList(data, current, (id) => void open(id)));
    dom.home.textContent = data.config.home;
    dom.home.title = data.config.home;
    dom.stamp.textContent = formatStamp(data.generatedAt);
  } catch (error) {
    // Failing to fetch data is normal (app-server just started, or the process died),
    // so show it as one thing instead of throwing into a console nobody sees.
    dom.overview.replaceChildren(failureOf(error));
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

  let current = dom.cwd.value.trim();
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
      dom.cwd.value = listing.path;
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

async function submit(): Promise<void> {
  const text = dom.input.value.trim();
  if ((text === "" && staged.length === 0) || busy) return;

  // Upload attachments first. Images ride as real attachments (the turn embeds the bytes —
  // the model sees pixels); other files keep the "path in text" route (the model opens them
  // with read_file). Upload order is preserved, so message text maps one-to-one to files.
  let full = text;
  let attachments: TurnAttachment[] | null = null;
  if (staged.length > 0) {
    const home = dom.home.textContent ?? "";
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
  const bubble = el("div", { class: "bubble streaming" });
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
    // Working directory for this turn (session-level working-directory): empty string = unset.
    const turnCwd = dom.cwd.value.trim();
    for await (const notification of talk({
      sessionId: current,
      message: text,
      stream: true,
      cwd: turnCwd === "" ? null : turnCwd,
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
          // Incremental insert: appending one text node per delta, never repainting the
          // whole bubble — a several-thousand-token reply must not reflow on every chunk.
          bubble.append(document.createTextNode(notification.delta));
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
          dom.goalState.textContent = `${notification.status}（第 ${notification.round}/${notification.maxRounds} 轮）`;
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
      bubble.textContent = completed.reply === "" ? "（这一轮没有回复）" : completed.reply;
      foot.textContent = turnFooter(completed);
    } else if (streamed !== "") {
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
 * Switch to a session: change the name, pull back the history, get ready to talk there.
 *
 * No switching while streaming — that turn's bytes would otherwise pour into another session.
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

function setBusy(value: boolean): void {
  busy = value;
  dom.send.disabled = value;
  dom.composer.classList.toggle("busy", value);
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
  if (node === null) throw new Error(`页面里没有 #${id}`);
  return node as T;
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
