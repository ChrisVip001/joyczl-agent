// 驾驶舱的主程序：开机把当前会话的历史拉回来，然后等着说话。
//
// 页面本身**没有状态**：会话、记忆、配置都在 Rust 那边（app-server 背后是
// state.db）。这里只有「在看哪个会话」和「这一轮正在流」这两个临时的视图状态，
// 刷新一下就回到默认。也就是说，浏览器崩了、标签页关了，什么都不丢 ——
// 这是这套分层最值钱的地方。

import type { TurnCompletedNotification } from "./protocol";
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
  toolCard,
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
};

let busy = false;

/**
 * 暂存区（「引用优先」）：选中的文件先摆在这儿，发送时才上传、
 * 把绝对路径注进消息 —— 引用文件和读文件是两件事，模型自己决定读不读。
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
 * 右侧面板的导航（数据即导航）。
 * 概览/记忆随整屏轮询走；用量/审批/定时按需取 —— 只刷**当前开着**的 tab，
 * 不为看不见的面板付请求。
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
  void refreshPanels();
}

/** 按需面板：只有当前 tab 是那三个新面板之一时才发请求。 */
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

// --- 运行状态三态 -----------------------
//
// 「等你说」是静止行；「思考中/回复中」shimmer + 秒表；「干活中」带工具名。
// 区分的是「卡住了」和「在跑」—— 秒表在动就是后者。

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
    // 从 thinking 换到 working：重置计时起点，让秒表对得上这一步。
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
 * 刷模型面板：**应答里没有密钥**，页面只显示「当前是哪套、key 存在哪」。
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

    // 头部的快速切换（pi 的 /model 形态）：眼前一点就能切，不用进面板。
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
 * 保存（并切换）。密钥那一栏填 `env:变量名` 就只读环境变量、不存下来；
 * 填真正的 key 则优先交给系统钥匙串。
 */
/**
 * 切到某套已配置的模型：只带 id，其余留空 = 保留原样
 * （不该为了切换重发一次密钥）。头部下拉与面板共用。
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

  // 右侧 tab：数据即导航 —— 每个按钮只带一个 tab 名，行为全在 switchTab。
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
    // Enter 发送，Shift+Enter 换行 —— 聊天框的老规矩。
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void submit();
    }
  });

  setInterval(() => {
    // 正在流的时候别刷：面板的数字会跳，但气泡里的字更重要。
    if (!busy) {
      void refresh();
      void refreshPanels();
    }
  }, POLL_MS);

  await refresh();
  await open(current);
  // 目标条：设/撤都走同一处（`goal/set`）—— 与终端里的 `/goal` 是同一件事。
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

  // 工作目录的「浏览」按钮：像桌面应用那样选文件夹（服务端代劳列目录）。
  const cwdBrowse = el("button", {
    class: "ghost",
    text: "浏览…",
    title: "选一个文件夹作为工作目录（也可以直接在左边手输）",
    type: "button",
  });
  cwdBrowse.addEventListener("click", () => void cwdPicker());
  dom.cwd.before(cwdBrowse);

  // 附件：选了先摆暂存区，发送时才上传 —— 中途反悔不用付任何代价。
  dom.attach.addEventListener("click", () => dom.file.click());
  dom.file.addEventListener("change", () => {
    for (const file of dom.file.files ?? []) staged.push(file);
    dom.file.value = "";
    renderStaged();
  });

  // 头部快速切换：与面板下拉同一处实现（saveModel 只带 id = 保留原配置）。
  dom.model.addEventListener("change", () => {
    void switchModel(dom.model.value).then(() => refreshModels());
  });

  // 模型面板：切换、保存、删除都走 `model/*` —— 与终端里的 /model 是同一处实现。
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

/** 把一屏数据画上去。 */
async function refresh(): Promise<void> {
  try {
    const data = await fetchDashboard();
    dom.overview.replaceChildren(renderOverview(data, current, (id) => void open(id)));
    dom.memory.replaceChildren(renderMemory(data));
    dom.home.textContent = data.config.home;
    dom.home.title = data.config.home;
    dom.stamp.textContent = formatStamp(data.generatedAt);
  } catch (error) {
    // 拉不到数据是常态（app-server 刚起来、进程挂了），
    // 所以显示成一件事，而不是抛到控制台里没人看见。
    dom.overview.replaceChildren(failureOf(error));
  }
}

/**
 * 文件夹选择器：服务端列目录，弹层里下钻，
 * 「选这里」把绝对路径填进顶栏的输入框。Esc 或取消即走，不强制。
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
  // 点开头的目录默认藏，底部有固定开关。
  let showHidden = false;
  // 「最近使用」：打开弹层时取一次（选定后会刷新）。
  let recents = await fetchRecents().catch(() => [] as string[]);

  /** 祖先链每一跳都可点。非「/」开头的路径给不出链，退化为纯文本。 */
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
    // 「最近使用」区：常去的目录不用再翻 —— 一键直达。
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

    // 底部动作条：新建文件夹 + 隐藏开关 + 取消。
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
      // 记进「最近使用」—— 下次打开弹层直接在顶部。
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

  // 附件先上传（顺序传，好让消息里的路径与文件一一对应）；
  // 注的是绝对路径 —— turn 可能带自己的工作目录，相对路径会指错地方。
  let full = text;
  if (staged.length > 0) {
    const home = dom.home.textContent ?? "";
    const lines: string[] = [];
    try {
      for (const file of staged) {
        const path = await uploadFile(file);
        lines.push(`（附件：${home}/${path}）`);
      }
    } catch (error: unknown) {
      // 上传失败不算世界末日：暂存区留着，让人能重试或拿掉。
      dom.staged.replaceChildren(failureOf(error));
      return;
    }
    full = `${lines.join("\n")}${text === "" ? "" : `\n${text}`}`;
  }

  dom.input.value = "";
  staged.length = 0;
  renderStaged();
  await send(full);
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
  let firstDelta = true;
  // 当前在跑的那张工具卡（串行循环里同时最多一张）与它的秒表。
  let currentCard: HTMLElement | null = null;
  let cardTimer: number | null = null;
  let cardStart = 0;
  showStatus("thinking", "思考中");

  try {
    // 本轮工作目录（会话级 working-directory）：空串当没设。
    const turnCwd = dom.cwd.value.trim();
    for await (const notification of talk({
      sessionId: current,
      message: text,
      stream: true,
      cwd: turnCwd === "" ? null : turnCwd,
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

        case "goalRound":
          // 目标循环续了一轮：把「为什么还没结束」说出来，否则界面自己动了一轮
          // 却没人知道为什么。
          trace.append(
            chip(
              `目标 · 第 ${notification.round}/${notification.maxRounds} 轮 · ${notification.status} — ${notification.reason}`,
              "gate",
            ),
          );
          dom.goalState.textContent = `${notification.status}（第 ${notification.round}/${notification.maxRounds} 轮）`;
          break;

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

        case "toolStarted": {
          // 工具卡片：人话摘要 + 实时秒表，
          // 不把 args 的 JSON 原样吐出来。
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
          // 结束本身就是那根卡片的句点：状态上色 + 定格耗时。
          const done = chip(
            `${notification.tool} ${notification.status === "ok" ? "好了" : "出错"}${
              notification.durationMs === null ? "" : ` · ${formatMs(notification.durationMs)}`
            }`,
            "tool",
          );
          done.classList.add(notification.status === "ok" ? "done-ok" : "done-bad");
          // 出错时把输出摘一句出来 —— 工具层的失败不中断 turn，
          // 但人应该当场看到失败原因，而不是等最后的 Error。
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
    hideStatus();
    if (cardTimer !== null) window.clearInterval(cardTimer);

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
