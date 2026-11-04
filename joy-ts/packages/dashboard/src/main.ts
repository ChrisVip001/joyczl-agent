// The dashboard's main program: on boot, pull back the current session's history, then wait for input.
//
// The page itself is **stateless**: sessions, memory, and config all live on the Rust side
// (behind app-server is state.db). The only temporary view state here is "which session is being
// viewed" and "whether a turn is streaming"; a refresh returns to defaults. In other words, if the
// browser crashes or the tab closes, nothing is lost — that's the most valuable part of this layering.

import type { TurnAttachment, TurnCompletedNotification } from "./protocol";
import {
  fetchApprovals,
  fetchDashboard,
  fetchMessages,
  fetchModelProfiles,
  fetchSchedules,
  fetchUsage,
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
  toolCard,
  turnFooter,
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

function renderStaged(): void {
  dom.staged.hidden = staged.length === 0;
  const nodes = staged.map((file, index) => {
    const item = el("span", { class: "chip", text: file.name });
    const drop = el("button", {
      class: "ghost",
      text: "×",
      title: "拿掉这个文件",
    });
    drop.addEventListener("click", () => {
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
  if (statusTimer !== null) {
    window.clearInterval(statusTimer);
    statusTimer = null;
  }
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
    const data = await fetchDashboard();
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

  dom.input.value = "";
  staged.length = 0;
  renderStaged();
  await send(full, attachments);
}

/** Say one thing, painting the course of this turn. */
async function send(text: string, attachments: TurnAttachment[] | null = null): Promise<void> {
  setBusy(true);

  dom.messages.append(el("div", { class: "msg me" }, el("div", { class: "bubble", text })));

  const trace = el("div", { class: "trace" });
  const bubble = el("div", { class: "bubble streaming" });
  const foot = el("div", { class: "foot" });
  dom.messages.append(el("div", { class: "msg joy" }, trace, bubble, foot));
  scrollToEnd();

  let streamed = "";
  let completed: TurnCompletedNotification | null = null;
  let firstDelta = true;
  // The tool card currently running (at most one in the serial loop) and its stopwatch.
  let currentCard: HTMLElement | null = null;
  let cardTimer: number | null = null;
  let cardStart = 0;
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
          break;

        case "textDelta":
          if (firstDelta) {
            firstDelta = false;
            showStatus("thinking", "回复中");
          }
          streamed += notification.delta;
          bubble.textContent = streamed;
          scrollToEnd();
          break;

        case "gateDecided":
          // The gate decides whether to consult memory. This line shows "why it remembered / not".
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
          // Asking for execution permission: buttons given, nothing moves until clicked — deny by default.
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

        case "goalRound":
          // The goal loop ran another round: say "why it hasn't ended yet", otherwise the UI
          // moved a round on its own and nobody knows why.
          trace.append(
            chip(
              `目标 · 第 ${notification.round}/${notification.maxRounds} 轮 · ${notification.status} — ${notification.reason}`,
              "gate",
            ),
          );
          dom.goalState.textContent = `${notification.status}（第 ${notification.round}/${notification.maxRounds} 轮）`;
          break;

        case "retry":
          // Retries are never silent: this line shows "those seconds weren't a hang".
          trace.append(
            chip(
              `重试第 ${notification.attempt} 次（${notification.reason}，${formatMs(
                notification.delayMs,
              )} 后）`,
              "gate",
            ),
          );
          break;

        case "toolStarted": {
          // Tool card: a plain-language summary + a live stopwatch,
          // without spitting out the args JSON as-is.
          showStatus("working", notification.tool);
          const card = toolCard(notification);
          const timeNode = card.querySelector(".toolcard-time");
          cardStart = Date.now();
          cardTimer = window.setInterval(() => {
            if (timeNode !== null) timeNode.textContent = elapsedSince(cardStart);
          }, 500);
          currentCard = card;
          trace.append(card);
          scrollToEnd();
          break;
        }

        case "toolCompleted": {
          if (cardTimer !== null) {
            window.clearInterval(cardTimer);
            cardTimer = null;
          }
          currentCard = null;
          // The end is the card's period: status coloring + frozen duration.
          const done = chip(
            `${notification.tool} ${notification.status === "ok" ? "好了" : "出错"}${
              notification.durationMs === null ? "" : ` · ${formatMs(notification.durationMs)}`
            }`,
            "tool",
          );
          done.classList.add(notification.status === "ok" ? "done-ok" : "done-bad");
          // On error, lift one sentence of output — a tool-level failure doesn't abort the turn,
          // but the failure reason should be visible now, not in the final Error.
          if (notification.status !== "ok" && notification.output !== "") {
            done.title = notification.output;
          }
          trace.append(done);
          scrollToEnd();
          break;
        }

        case "consolidationCompleted":
          if (notification.newFacts > 0) {
            trace.append(chip(`记住了 ${notification.newFacts} 条新事实`, "note"));
            void refresh(); // The memory panel catches up now, no need to wait for the next poll
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
    // Wrap-up lives in finally: even a mid-way error must clear the "writing" state,
    // otherwise the blinking cursor would stay on the bubble forever.
    bubble.classList.remove("streaming");
    hideStatus();
    if (cardTimer !== null) window.clearInterval(cardTimer);

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

    scrollToEnd();
    setBusy(false);
    // This turn changed the session's message count and last-active time; refresh the panels.
    void refresh();
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
      ...(nodes.length === 0 ? [muted("这个会话还没说过话。说第一句就有了。")] : nodes),
    );
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

function scrollToEnd(): void {
  dom.messages.scrollTop = dom.messages.scrollHeight;
}

function must<T extends HTMLElement = HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  // Blow up on the spot when not found. Silently doing nothing is far harder to debug —
  // and the only possible cause is index.html and this getting out of sync.
  if (node === null) throw new Error(`页面里没有 #${id}`);
  return node as T;
}

void boot();
