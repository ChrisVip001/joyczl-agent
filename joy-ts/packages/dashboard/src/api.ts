// app-server 的 HTTP 面，三个端点。
//
// 这里不引入客户端库：这几个端点的形状简单到不值得再包一层 —— 多一层抽象
// 就多一层要读、要跟着协议改的代码。形状对不对是**编译期**的事，因为类型
// 直接来自生成物（`./protocol.ts`），不是手抄的。

import type {
  ApprovalRespondParams,
  ApprovalRespondResponse,
  DashboardData,
  GoalSetParams,
  GoalSetResponse,
  ErrorObject,
  ModelProfilesResponse,
  ModelRemoveParams,
  ModelRemoveResponse,
  ModelSaveParams,
  ModelSaveResponse,
  ApprovalLogResponse,
  ScheduleListResponse,
  ServerNotification,
  UsageSummaryResponse,
  SessionMessagesResponse,
  TurnStartParams,
} from "./protocol";

/** `/api/fs` 的一页目录列表（ops 本地视图，不进协议）。 */
export interface FsListing {
  path: string;
  parent: string | null;
  entries: { name: string; path: string }[];
}

/** app-server 说的话，原样带出来：`code` 是协议里的错误码，`message` 是给人看的。 */
export class ApiError extends Error {
  readonly code: number;

  constructor(error: ErrorObject) {
    super(error.message);
    this.name = "ApiError";
    this.code = error.code;
  }
}

/**
 * `/api/*` 的统一入口：自动带上部署面的 Bearer token。
 *
 * 服务端设了 `JOY_OPS_TOKEN` 时，每个 API 请求都要带
 * `Authorization: Bearer …`；token 存 localStorage（这是**部署面**凭据，
 * 与模型 key 是两回事）。收到 401 就问一次 —— 用 `prompt` 而不是一套
 * DOM：这条路一个部署只走一次，问完存下来就再也不见了。
 */
async function apiFetch(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers);
  const token = localStorage.getItem("joy-ops-token");
  if (token) headers.set("authorization", `Bearer ${token}`);

  const response = await fetch(path, { ...init, headers });
  if (response.status === 401) {
    const entered = window.prompt("这台 Joy 的 API 需要 token（服务端的 JOY_OPS_TOKEN）");
    if (entered) {
      localStorage.setItem("joy-ops-token", entered);
      const retryHeaders = new Headers(init.headers);
      retryHeaders.set("authorization", `Bearer ${entered}`);
      return fetch(path, { ...init, headers: retryHeaders });
    }
  }
  return response;
}

/** 首屏数据。失败时抛 `ApiError`。 */
export async function fetchDashboard(): Promise<DashboardData> {
  const response = await apiFetch("/api/data", { headers: { accept: "application/json" } });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as DashboardData;
}

/**
 * 一个会话说过的话。
 *
 * `cursor` 给了就取比它**更早**的一页 —— 这就是「往上翻旧账」。不给就是最近的
 * 一页，一页多少条由服务端决定（`limit` 留在协议里，页面用不到）。
 */
export async function fetchMessages(
  sessionId: string,
  cursor: string | null = null,
): Promise<SessionMessagesResponse> {
  const query = new URLSearchParams({ sessionId });
  if (cursor !== null) query.set("cursor", cursor);

  const response = await apiFetch(`/api/session?${query.toString()}`, {
    headers: { accept: "application/json" },
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as SessionMessagesResponse;
}

/**
 * 说一句话，把这一轮的通知一条条吐出来。
 *
 * 用 `fetch` + 自己读 body，而不是 `EventSource`：EventSource 只能发 GET，
 * 而这个请求要带一段消息。SSE 的帧格式简单到不值得再拉一个库 ——
 * 空行分隔，`data:` 后面是载荷。
 *
 * 一定要传 `stream: true`：`textDelta` 只在流式时才发（脚本调用默认关着，
 * 那里不需要逐字）。
 */
export async function* talk(params: TurnStartParams): AsyncGenerator<ServerNotification> {
  const response = await apiFetch("/api/turn", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "text/event-stream" },
    body: JSON.stringify(params),
  });
  if (!response.ok || response.body === null) throw new ApiError(await errorOf(response));

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });

      let cut = buffer.indexOf("\n\n");
      while (cut >= 0) {
        const payload = dataOf(buffer.slice(0, cut));
        buffer = buffer.slice(cut + 2);
        if (payload !== null) yield payload;
        cut = buffer.indexOf("\n\n");
      }
    }
  } finally {
    // 提前离开（这一轮完了、或者用户切走了）就把连接放掉，
    // 否则浏览器会攥着一个没人读的流，直到它自己超时。
    await reader.cancel().catch(() => undefined);
  }
}

/**
 * 设或清一个目标。`condition` 为空 = 清除。
 *
 * 目标是**只有人**能设的东西：模型没有这条路径，所以「模型给自己派活」在这里不是
 * 被禁止，而是不存在。
 */
export async function setGoal(params: GoalSetParams): Promise<GoalSetResponse> {
  const response = await apiFetch("/api/goal", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(params),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as GoalSetResponse;
}

/**
 * 回答一次「要不要执行」。
 *
 * 与 turn 不同，这是一问一答：答完就没了，所以不用流。`accepted: false` 表示
 * 太晚送达（那一轮早就不等了）—— 页面上要说一句，别让人以为自己的点击算数了。
 */
export async function respondApproval(
  params: ApprovalRespondParams,
): Promise<ApprovalRespondResponse> {
  const response = await apiFetch("/api/approval", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(params),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as ApprovalRespondResponse;
}

/** 一帧里 `data:` 后面的东西。keep-alive 那种注释帧返回 null。 */
function dataOf(frame: string): ServerNotification | null {
  const payload = frame
    // \r 是因为有的代理会把换行改掉；它出现在行尾，不影响载荷本身。
    .split("\n")
    .map((line) => line.replace(/\r$/, ""))
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice("data:".length).trimStart())
    .join("");

  if (payload === "") return null;
  return JSON.parse(payload) as ServerNotification;
}

/**
 * 从失败的响应里取出错误。
 *
 * 出错时 body 是协议里的 `ErrorObject`；但万一不是（比如 axum 自己挡下来的
 * 422、或者服务没起来），也得给出一句能看懂的话 —— 把 JSON 解析异常直接
 * 抛到界面上，用户看到的就是一串英文堆栈。
 */
async function errorOf(response: Response): Promise<ErrorObject> {
  const text = await response.text();
  try {
    const parsed = JSON.parse(text) as Partial<ErrorObject>;
    if (typeof parsed.message === "string" && typeof parsed.code === "number") {
      return parsed as ErrorObject;
    }
  } catch {
    // 不是 JSON，走下面那句兜底。
  }
  return {
    code: response.status,
    message: text.trim() || `HTTP ${response.status} ${response.statusText}`,
  };
}

/**
 * 已配置的多套模型 + 当前那套。
 *
 * 应答里**没有密钥**，只有「key 存在哪」的指针 —— 浏览器不该拿到密钥。
 */
export async function fetchModelProfiles(): Promise<ModelProfilesResponse> {
  const response = await apiFetch("/api/model/profiles", {
    headers: { accept: "application/json" },
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as ModelProfilesResponse;
}

/**
 * 新增/改一套并切成当前；只给 `id` + `makeActive` 就是**切换**（不用重发密钥）。
 *
 * `keyStoredIn` 会如实说密钥落在哪（keychain / file / env:VAR），页面照抄给用户看。
 */
export async function saveModel(params: ModelSaveParams): Promise<ModelSaveResponse> {
  const response = await apiFetch("/api/model/save", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(params),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as ModelSaveResponse;
}

/** 删一套，连它的凭据一起。 */
export async function removeModel(params: ModelRemoveParams): Promise<ModelRemoveResponse> {
  const response = await apiFetch("/api/model/remove", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(params),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as ModelRemoveResponse;
}

/** 用量台账（迁移 0010）：per-model 聚合 + 总数。只记账不强制。 */
export async function fetchUsage(): Promise<UsageSummaryResponse> {
  const response = await apiFetch("/api/usage", { headers: { accept: "application/json" } });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as UsageSummaryResponse;
}

/** 审批审计（迁移 0011）：最近 100 条「问了才跑」，新的在前。 */
export async function fetchApprovals(): Promise<ApprovalLogResponse> {
  const response = await apiFetch("/api/approvals", { headers: { accept: "application/json" } });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as ApprovalLogResponse;
}

/**
 * 上传一个文件，返回相对 home 的路径。
 *
 * 通道是原始字节而不是 multipart：单文件用不着信封，服务端也少一个解析器。
 * 落盘名由服务端生成（用户输入进不了路径）；返回的路径前端会拼上 home
 * 变成绝对路径注进消息 —— 模型用 read_file 自己读（引用与读取解耦）。
 */
export async function uploadFile(file: File): Promise<string> {
  const query = new URLSearchParams({ name: file.name });
  const response = await apiFetch(`/api/upload?${query.toString()}`, {
    method: "POST",
    headers: { "content-type": "application/octet-stream" },
    body: file,
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  const parsed = (await response.json()) as { path: string };
  return parsed.path;
}

/**
 * 服务端目录浏览（选工作目录用）。
 *
 * 浏览器的文件夹选择器拿不到绝对路径，所以「像桌面应用那样选文件夹」
 * 由知道文件系统的服务端代劳 —— 只读、只列子目录。
 */
export async function fetchFs(path: string | null, hidden = false): Promise<FsListing> {
  const query = new URLSearchParams();
  if (path) query.set("path", path);
  if (hidden) query.set("hidden", "1");
  const response = await apiFetch(`/api/fs?${query.toString()}`, {
    headers: { accept: "application/json" },
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as FsListing;
}

/**
 * 在 parent 下新建一个子目录：选择器里
 * 「选目录」途中就能建目录，不用跳出弹层去开终端。名字纪律在服务端。
 */
export async function createFolder(parent: string, name: string): Promise<string> {
  const query = new URLSearchParams({ path: parent, name });
  const response = await apiFetch(`/api/fs/mkdir?${query.toString()}`, {
    method: "POST",
    headers: { accept: "application/json" },
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  const parsed = (await response.json()) as { path: string };
  return parsed.path;
}

/** 「最近使用」的工作目录（记住、直达）。 */
export interface Recents {
  paths: string[];
}

/** 最近用过的目录（服务端持久化在 home/workspaces.json）。 */
export async function fetchRecents(): Promise<string[]> {
  const response = await apiFetch("/api/fs/recents", {
    headers: { accept: "application/json" },
  });
  if (!response.ok) return [];
  return ((await response.json()) as Recents).paths;
}

/** 选定一个目录：记进「最近使用」。失败不碍事 —— 便利品没有资格报警。 */
export async function recordRecent(path: string): Promise<void> {
  try {
    await apiFetch(`/api/fs/recents?path=${encodeURIComponent(path)}`, {
      method: "POST",
      headers: { accept: "application/json" },
    });
  } catch {
    /* 忽略：下次 turn 用了它还会记。 */
  }
}

/** 调度状态快照（迁移 0012）：每任务的 last_fired/claimed/catch_up/delivery。 */
export async function fetchSchedules(): Promise<ScheduleListResponse> {
  const response = await apiFetch("/api/schedules", { headers: { accept: "application/json" } });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as ScheduleListResponse;
}
