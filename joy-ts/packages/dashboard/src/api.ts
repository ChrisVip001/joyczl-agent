// app-server 的 HTTP 面，三个端点。
//
// 这里不引入客户端库：这几个端点的形状简单到不值得再包一层 —— 多一层抽象
// 就多一层要读、要跟着协议改的代码。形状对不对是**编译期**的事，因为类型
// 直接来自生成物（`./protocol.ts`），不是手抄的。

import type {
  DashboardData,
  ErrorObject,
  ServerNotification,
  SessionMessagesResponse,
  TurnStartParams,
} from "./protocol";

/** app-server 说的话，原样带出来：`code` 是协议里的错误码，`message` 是给人看的。 */
export class ApiError extends Error {
  readonly code: number;

  constructor(error: ErrorObject) {
    super(error.message);
    this.name = "ApiError";
    this.code = error.code;
  }
}

/** 首屏数据。失败时抛 `ApiError`。 */
export async function fetchDashboard(): Promise<DashboardData> {
  const response = await fetch("/api/data", { headers: { accept: "application/json" } });
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

  const response = await fetch(`/api/session?${query.toString()}`, {
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
  const response = await fetch("/api/turn", {
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
