// The HTTP face of app-server — three endpoints.
//
// No client library here: these endpoints are simple enough that wrapping them isn't worth it —
// one more abstraction is one more layer to read and to track with the protocol. Shape checks
// are **compile-time**, because the types come from the generated artifacts (`./protocol.ts`).

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
  TurnInterruptParams,
  TurnInterruptResponse,
  TurnStartParams,
} from "./protocol";

/** One page of the `/api/fs` directory listing (ops's local view, not in the protocol). */
export interface FsListing {
  path: string;
  parent: string | null;
  entries: { name: string; path: string }[];
}

/** What app-server said, as-is: `code` is the protocol error code, `message` is for humans. */
export class ApiError extends Error {
  readonly code: number;

  constructor(error: ErrorObject) {
    super(error.message);
    this.name = "ApiError";
    this.code = error.code;
  }
}

/**
 * The unified entry for `/api/*`: automatically attaches the deployment-plane Bearer token.
 *
 * When the server sets `JOY_OPS_TOKEN`, every API request must carry `Authorization: Bearer …`;
 * the token is stored in localStorage (a **deployment-plane** credential, a different thing
 * from model keys). On a 401 it asks once — using `prompt` instead of a DOM: this path is
 * walked once per deployment; ask, save, and it's never seen again.
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

/** First-screen data. Throws `ApiError` on failure. */
export async function fetchDashboard(): Promise<DashboardData> {
  const response = await apiFetch("/api/data", { headers: { accept: "application/json" } });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as DashboardData;
}

/**
 * What one session has said.
 *
 * Given a `cursor`, fetch the page **earlier** than it — "paging back through older messages".
 * Without it, the latest page; page size is server-decided (`limit` stays in the protocol).
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
 * Say one thing, streaming out this turn's notifications one by one.
 *
 * Uses `fetch` + reading the body manually instead of `EventSource`: EventSource can only send
 * GET, while this request must carry a message. SSE's frame format is simple enough not to
 * pull in a library — blank-line separated, with the payload after `data:`.
 *
 * Always pass `stream: true`: `textDelta` is only sent when streaming (scripts default off,
 * where word-by-word isn't needed).
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
    // Leaving early (turn finished, or the user navigated away) releases the connection,
    // otherwise the browser would hold an unread stream until it times out on its own.
    await reader.cancel().catch(() => undefined);
  }
}

/**
 * Set or clear a goal. Empty `condition` = clear.
 *
 * A goal is **only human-settable**: the model has no such path, so "the model assigning
 * itself work" isn't forbidden here — it simply doesn't exist.
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
 * Answer one "should this execute".
 *
 * Unlike a turn, this is one question one answer: gone once answered, so no stream.
 * `accepted: false` = arrived too late (that round stopped waiting) — say so on the page.
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

/** What follows `data:` in one frame. Keep-alive-style comment frames return null. */
function dataOf(frame: string): ServerNotification | null {
  const payload = frame
    // \r exists because some proxies rewrite line endings; at end of line it doesn't affect the payload.
    .split("\n")
    .map((line) => line.replace(/\r$/, ""))
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice("data:".length).trimStart())
    .join("");

  if (payload === "") return null;
  return JSON.parse(payload) as ServerNotification;
}

/**
 * Extract the error from a failed response.
 *
 * On error the body is the protocol's `ErrorObject`; but if it isn't (say a 422 axum itself
 * blocked, or the service isn't up), a comprehensible line is still needed — throwing the
 * JSON parse exception straight to the UI would show the user a stack of English.
 */
async function errorOf(response: Response): Promise<ErrorObject> {
  const text = await response.text();
  try {
    const parsed = JSON.parse(text) as Partial<ErrorObject>;
    if (typeof parsed.message === "string" && typeof parsed.code === "number") {
      return parsed as ErrorObject;
    }
  } catch {
    // Not JSON; fall through to the fallback below.
  }
  return {
    code: response.status,
    message: text.trim() || `HTTP ${response.status} ${response.statusText}`,
  };
}

/**
 * The configured model profiles + the current one.
 *
 * **No keys in the response**, only a pointer to "where the key is stored" — not for browsers.
 */
export async function fetchModelProfiles(): Promise<ModelProfilesResponse> {
  const response = await apiFetch("/api/model/profiles", {
    headers: { accept: "application/json" },
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as ModelProfilesResponse;
}

/**
 * Add/change a profile and make it current; only `id` + `makeActive` = **switch** (no key sent).
 *
 * `keyStoredIn` says where the key landed (keychain / file / env:VAR); the page relays it.
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

/** Delete a profile, along with its credentials. */
export async function removeModel(params: ModelRemoveParams): Promise<ModelRemoveResponse> {
  const response = await apiFetch("/api/model/remove", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(params),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as ModelRemoveResponse;
}

/**
 * Stop a running turn. `interrupted: false` = arrived too late (it had already finished) —
 * the UI says so instead of pretending something was stopped.
 */
export async function interruptTurn(params: TurnInterruptParams): Promise<TurnInterruptResponse> {
  const response = await apiFetch("/api/interrupt", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(params),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as TurnInterruptResponse;
}

/** Usage ledger (migration 0010): per-model aggregation + totals. Bookkeeping only. */
export async function fetchUsage(): Promise<UsageSummaryResponse> {
  const response = await apiFetch("/api/usage", { headers: { accept: "application/json" } });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as UsageSummaryResponse;
}

/** Approval audit (migration 0011): the latest 100 "asked-before-run" entries, newest first. */
export async function fetchApprovals(): Promise<ApprovalLogResponse> {
  const response = await apiFetch("/api/approvals", { headers: { accept: "application/json" } });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as ApprovalLogResponse;
}

/**
 * Upload one file, returning the path relative to home.
 *
 * The channel is raw bytes rather than multipart: a single file needs no envelope, and the
 * server skips a parser. The name is server-generated (user input can't enter the path); the
 * frontend prepends home and injects the absolute path — the model reads it with read_file.
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
 * Server-side directory browsing (for picking the working directory).
 *
 * The browser's folder picker can't get absolute paths, so "picking a folder like a desktop
 * app" is done by the server, which knows the filesystem — read-only, subdirectories only.
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
 * Create a subdirectory under parent: a directory can be created mid-"pick" in the selector,
 * without jumping out of the overlay to open a terminal. Name discipline lives on the server.
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

/** "Recently used" working directories (remembered, one-click). */
export interface Recents {
  paths: string[];
}

/** Recently used directories (persisted server-side in home/workspaces.json). */
export async function fetchRecents(): Promise<string[]> {
  const response = await apiFetch("/api/fs/recents", {
    headers: { accept: "application/json" },
  });
  if (!response.ok) return [];
  return ((await response.json()) as Recents).paths;
}

/** Record a chosen directory into "Recents". Failure doesn't matter — a convenience has no alarm. */
export async function recordRecent(path: string): Promise<void> {
  try {
    await apiFetch(`/api/fs/recents?path=${encodeURIComponent(path)}`, {
      method: "POST",
      headers: { accept: "application/json" },
    });
  } catch {
    /* Ignore: it'll be recorded again the next time a turn uses it. */
  }
}

/** Scheduling state snapshot (migration 0012): per-job last_fired/claimed/catch_up/delivery. */
export async function fetchSchedules(): Promise<ScheduleListResponse> {
  const response = await apiFetch("/api/schedules", { headers: { accept: "application/json" } });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as ScheduleListResponse;
}
