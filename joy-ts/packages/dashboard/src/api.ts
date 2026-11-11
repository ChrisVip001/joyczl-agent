// The HTTP face of app-server — three endpoints.
//
// No client library here: these endpoints are simple enough that wrapping them isn't worth it —
// one more abstraction is one more layer to read and to track with the protocol. Shape checks
// are **compile-time**, because the types come from the generated artifacts (`./protocol.ts`).

import type {
  ApprovalRespondParams,
  ApprovalRespondResponse,
  DashboardData,
  ErrorObject,
  ServerNotification,
  InboxSummarizeResponse,
  SessionMessagesResponse,
  SessionGetResponse,
  SessionNewParams,
  SessionNewResponse,
  SessionUpdateParams,
  SessionUpdateResponse,
  TaskCreateParams,
  TaskEnvelope,
  TurnInterruptParams,
  TurnInterruptResponse,
  TurnStartParams,
  TurnSteerParams,
  TurnSteerResponse,
  TurnFollowUpParams,
  TurnFollowUpResponse,
  TurnListResponse,
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
 * The remote Joy server, or "" for same-origin (the local `joy dashboard`).
 *
 * Empty = the app was opened on its own server (the common case). Non-empty = the page
 * is a client of a remote hub (Tauri shell, a PWA on another origin) — every request is
 * prefixed, and 401 hands over to the login card instead of the one-time prompt.
 */
export function serverBase(): string {
  return localStorage.getItem("joy-server") ?? "";
}

export function setServerBase(base: string): void {
  const trimmed = base.trim().replace(/\/+$/, "");
  if (trimmed === "") localStorage.removeItem("joy-server");
  else localStorage.setItem("joy-server", trimmed);
}

/**
 * The unified entry for `/api/*`: attaches the Bearer token (account or deployment-plane —
 * both travel the same header) and prefixes the remote base when one is configured.
 *
 * Same-origin deployments keep the one-time `prompt` for `JOY_OPS_TOKEN` (walked once per
 * deployment; ask, save, never seen again). Remote deployments dispatch `joy:unauthorized`
 * — the login card listens for it, because a prompt can't explain "your session expired".
 */
async function apiFetch(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers);
  const token = localStorage.getItem("joy-ops-token");
  if (token) headers.set("authorization", `Bearer ${token}`);

  const response = await fetch(serverBase() + path, { ...init, headers });
  if (response.status === 401) {
    if (serverBase() !== "") {
      window.dispatchEvent(new CustomEvent("joy:unauthorized"));
      return response;
    }
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

/** What register/login answers: the account name and this device's token. */
export interface AuthOutcome {
  user: string;
  token: string;
}

/**
 * Register or log in on the configured server.
 *
 * Deliberately *not* via `apiFetch`: these two endpoints are reachable without a token,
 * and their 401 ("wrong password") must surface as an inline error on the card — not as
 * "session expired" chatter.
 */
async function authPost(path: string, body: AuthBody): Promise<AuthOutcome> {
  const response = await fetch(serverBase() + path, {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as AuthOutcome;
}

interface AuthBody {
  username: string;
  password: string;
  device: string;
}

/** Create an account and pair this device in one step. */
export function registerAccount(params: AuthBody): Promise<AuthOutcome> {
  return authPost("/api/auth/register", params);
}

/** Verify the password and pair this device (same device → same token). */
export function loginAccount(params: AuthBody): Promise<AuthOutcome> {
  return authPost("/api/auth/login", params);
}

/**
 * First-screen data. `sessionId` names the viewed session — the per-session reads
 * (goal state, todo sheet) ride along only when it's given. Throws `ApiError`.
 */
export async function fetchDashboard(sessionId: string | null = null): Promise<DashboardData> {
  // The literal path stays a literal (smoke-dashboard greps the bundle for it).
  const path =
    sessionId === null ? "/api/data" : `/api/data?sessionId=${encodeURIComponent(sessionId)}`;
  const response = await apiFetch(path, { headers: { accept: "application/json" } });
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
 * Create a session that exists **before** its first message (0.15).
 *
 * The id comes back from the server: an id generated in the browser can collide with another
 * tab (or another device), and two conversations sharing one row is not a recoverable state.
 * A name and a working directory can be given right away, so the empty session is already
 * meaningful in the list instead of a placeholder the page invents locally.
 */
export async function createSession(params: SessionNewParams): Promise<SessionNewResponse> {
  const response = await apiFetch("/api/session/new", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(params),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as SessionNewResponse;
}

/** One session's record — used on switch, because the list is capped and may not contain it. */
export async function fetchSessionMeta(sessionId: string): Promise<SessionGetResponse> {
  const query = new URLSearchParams({ sessionId });
  const response = await apiFetch(`/api/session/meta?${query.toString()}`, {
    headers: { accept: "application/json" },
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as SessionGetResponse;
}

/**
 * Rename a session or rebind its working directory.
 *
 * `null`/absent leaves a field alone; `""` clears it. Those are two intents, not one.
 */
export async function updateSession(params: SessionUpdateParams): Promise<SessionUpdateResponse> {
  const response = await apiFetch("/api/session/update", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(params),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as SessionUpdateResponse;
}

/**
 * Add a sentence to the turn that is already running (0.16).
 *
 * Distinct from `talk` on purpose: starting a second turn would be refused (`SESSION_BUSY`) and
 * cancelling first would throw away tool work that already happened. `queued` counts everything
 * this turn has accepted, so the UI can say "the third one" instead of pretending it is first.
 */
export async function steerTurn(params: TurnSteerParams): Promise<TurnSteerResponse> {
  const response = await apiFetch("/api/turn/steer", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(params),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as TurnSteerResponse;
}

/**
 * Queue a message to run **after** the turn in flight (0.16).
 *
 * `steer` and this differ in what the person meant: "and also, while you are at it" versus
 * "when you are done". This one becomes its own turn, so it keeps its own rounds and budget —
 * and it runs even after this page is closed.
 */
export async function followUpTurn(
  params: TurnFollowUpParams,
): Promise<TurnFollowUpResponse> {
  const response = await apiFetch("/api/turn/follow-up", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(params),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as TurnFollowUpResponse;
}

/**
 * Turns running right now (all of them when `sessionId` is omitted).
 *
 * The page's own `busy` flag cannot answer this: another device (or a queued follow-up) may be
 * the one running the turn, and a reloaded tab has no memory of it.
 */
export async function fetchTurns(sessionId?: string): Promise<TurnListResponse> {
  const query = new URLSearchParams();
  if (sessionId !== undefined) query.set("sessionId", sessionId);
  const suffix = query.toString();
  const response = await apiFetch(suffix === "" ? "/api/turns" : `/api/turns?${suffix}`, {
    headers: { accept: "application/json" },
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as TurnListResponse;
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

  for await (const frame of frames(response.body)) {
    const payload = dataOf(frame);
    if (payload !== null) yield payload;
  }
}

/** One SSE frame (blank-line separated). Leaving early releases the connection. */
async function* frames(body: ReadableStream<Uint8Array>): AsyncGenerator<string> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });

      let cut = buffer.indexOf("\n\n");
      while (cut >= 0) {
        yield buffer.slice(0, cut);
        buffer = buffer.slice(cut + 2);
        cut = buffer.indexOf("\n\n");
      }
    }
  } finally {
    await reader.cancel().catch(() => undefined);
  }
}

/** What /api/events pushes: "something happened", the receiver pulls the details. */
export interface EventPing {
  kind: string;
  turnId?: string;
  sessionId?: string;
}

/**
 * The cross-device channel: GET /api/events, one ping per meaningful change elsewhere.
 *
 * EventSource can't carry the Bearer header, so this is a fetch + manual reader — the
 * same frames talk() speaks, quieter content.
 */
export async function* followEvents(): AsyncGenerator<EventPing> {
  const response = await apiFetch("/api/events", {
    headers: { accept: "text/event-stream" },
  });
  if (!response.ok || response.body === null) throw new ApiError(await errorOf(response));

  for await (const frame of frames(response.body)) {
    const payload = frameData(frame);
    if (payload !== null) yield payload as EventPing;
  }
}

/**
 * The long-task switches: create / suspend / resume / cancel.
 *
 * One door (`/api/task`) with an action field — the protocol keeps four methods, ops only
 * translates. **Human-only** like the goal: the model has no task tool.
 */
export type TaskActionBody =
  | ({ action: "create" } & TaskCreateParams)
  | { action: "suspend" | "resume" | "cancel"; taskId: string; reason?: string | null };

export async function taskAction(body: TaskActionBody): Promise<TaskEnvelope> {
  const response = await apiFetch("/api/task", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as TaskEnvelope;
}

/**
 * The human's "handle these": one turn over the unseen mail (0.13.0).
 *
 * Nothing happens on its own unless connectors.json says `inbound.mode = "turn"` — that
 * is a standing decision, and this button is the same decision made once.
 */
export async function summarizeInbox(sessionId?: string): Promise<InboxSummarizeResponse> {
  const response = await apiFetch("/api/inbox", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(sessionId === undefined ? {} : { sessionId }),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as InboxSummarizeResponse;
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

/** What follows `data:` in one frame, as raw JSON. Keep-alive-style comment frames return null. */
function frameData(frame: string): unknown {
  const payload = frame
    // \r exists because some proxies rewrite line endings; at end of line it doesn't affect the payload.
    .split("\n")
    .map((line) => line.replace(/\r$/, ""))
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice("data:".length).trimStart())
    .join("");

  if (payload === "") return null;
  return JSON.parse(payload);
}

function dataOf(frame: string): ServerNotification | null {
  const payload = frameData(frame);
  return payload === null ? null : (payload as ServerNotification);
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

// --- 能力中心（技能 / MCP 连接器）-----------------------------------------------
//
// Ops-local views, like /api/fs above: neither the SDK nor the terminal needs
// "browse the skill folders", so these shapes live here rather than in the protocol.
// `source` is the one field that matters: `user` rows can be managed, `builtin`
// rows are greyed out (JOY_SKILL_DIRS / JOY_MCP_CONFIG ship with the install).

/** One skill as the capability center shows it. */
export interface SkillRow {
  name: string;
  description: string;
  version: string | null;
  /** Present = also a scheduled job (see `joy schedule`). */
  schedule: string | null;
  /** false = explicit `$name` references only, never keyword-triggered. */
  allowModelInvocation: boolean;
  dependencies: string[];
  source: "user" | "builtin";
  folder: string;
}

/** One MCP connector as the capability center shows it. */
export interface McpRow {
  name: string;
  /** `url` (Streamable HTTP) or `command` (stdio) — the shape decides. */
  kind: "url" | "command";
  url: string | null;
  command: string | null;
  args: string[];
  /** `none` / `auth_env` (a variable name) / `oauth` (browser login). */
  auth: "none" | "auth_env" | "oauth";
  authEnv: string | null;
  source: "user" | "builtin";
}

export interface SkillsView {
  skills: SkillRow[];
  error: string | null;
}

export interface McpView {
  servers: McpRow[];
  error: string | null;
}

/** What every capability write answers: one line a person can read. */
export interface Outcome {
  ok: boolean;
  message: string;
}

/**
 * Turn one write response into an `Outcome`, or throw.
 *
 * A refusal answers **200** with the protocol's `ErrorObject` (the `/api/task`
 * convention: a human action refused is an answer, not a transport fault). So the
 * HTTP status cannot separate "done" from "refused" — `ok` is the only honest
 * discriminator, and checking it here keeps that rule in one place instead of
 * four call sites guessing.
 */
async function outcomeOf(response: Response): Promise<Outcome> {
  const body = (await response.json()) as Partial<Outcome> & Partial<ErrorObject>;
  if (body.ok !== true) {
    throw new ApiError({
      code: typeof body.code === "number" ? body.code : response.status,
      message: body.message ?? `HTTP ${response.status} ${response.statusText}`,
    });
  }
  return body as Outcome;
}

/** The skills Joy has loaded, user ones first, plus a read error if the folder is unreadable. */
export async function fetchSkills(): Promise<SkillsView> {
  const response = await apiFetch("/api/skills", {
    headers: { accept: "application/json" },
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as SkillsView;
}

/**
 * Install a skill: paste SKILL.md text, or give a URL (GitHub blob / Gist pages are
 * rewritten to raw content server-side). Validation is the same gate
 * `joy skill install` passes — the page cannot install what the CLI would refuse.
 */
export async function installSkill(body: {
  text?: string;
  url?: string;
}): Promise<Outcome> {
  const response = await apiFetch("/api/skills/install", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return outcomeOf(response);
}

/** Delete a user skill (the server refuses built-in ones). */
export async function removeSkill(name: string): Promise<Outcome> {
  const response = await apiFetch("/api/skills/remove", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify({ name }),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return outcomeOf(response);
}

/** The configured MCP connectors, user file first (first wins — the runtime's merge). */
export async function fetchMcp(): Promise<McpView> {
  const response = await apiFetch("/api/mcp", {
    headers: { accept: "application/json" },
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return (await response.json()) as McpView;
}

/** One server stanza for `POST /api/mcp/save` — the same shape mcp.json holds. */
export interface McpServerSpec {
  name: string;
  url?: string;
  command?: string;
  args?: string[];
  authEnv?: string;
}

/** Add or replace one connector in `<home>/mcp.json` (takes effect on next start). */
export async function saveMcpServer(spec: McpServerSpec): Promise<Outcome> {
  const response = await apiFetch("/api/mcp/save", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(spec),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return outcomeOf(response);
}

/** Delete a user connector (the server refuses built-in ones). */
export async function removeMcpServer(name: string): Promise<Outcome> {
  const response = await apiFetch("/api/mcp/remove", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify({ name }),
  });
  if (!response.ok) throw new ApiError(await errorOf(response));
  return outcomeOf(response);
}

