import { JoyClient, JoyError, METHODS, StdioTransport } from "@joy/client";
import type { ServerNotification } from "@joy/client";

/** What a completed turn leaves behind. */
export interface TurnOutcome {
  /** Joy's session id — it is `conversation` itself; see `sessionIdFor`. */
  sessionId: string;
  reply: string;
  /** Tools actually used this turn, in call order; repeats count. */
  tools: string[];
  /** The model that really answered. When graph routes to quick that's the small one — as-is. */
  model: string;
  iterations: number;
}

/** Movement while a turn runs. Adapters can use it to show "typing…". */
export interface ProgressNote {
  conversation: string;
  text: string;
}

/**
 * The single interface between an adapter and Joy: one message in, one reply out.
 *
 * A function rather than the `JoyBridge` itself, so adapters can be tested independently —
 * long polling, WebSocket heartbeats, message splitting, allowlists have nothing to do with Joy.
 *
 * Like `ProgressNote`, it belongs to the **platform-agnostic** half, so it lives here rather
 * than in one platform's adapter — otherwise Discord would import its own interface from the
 * Telegram file.
 */
export type Responder = (conversation: string, text: string) => Promise<string>;

export interface JoyBridgeOptions {
  /** `joy` binary path. When absent, found via the client's usual order. */
  command?: string;
  args?: string[];
  cwd?: string;
  /** Environment passed through to app-server (`JOY_HOME`, the vendors' API keys…). */
  env?: NodeJS.ProcessEnv;
  /**
   * An already-connected client. When absent, an app-server is started here.
   *
   * This seam exists for tests: queuing, notification folding, clearing wreckage after failure
   * are pure logic — proving them shouldn't need a real process, a model, or a network trip.
   */
  client?: JoyClient;
  /** The server's human words and local noise. Defaults to stderr. */
  onLog?: (text: string) => void;
  /** Progress within a turn. Dropped by default. */
  onProgress?: (note: ProgressNote) => void;
}

/**
 * Session ids derive directly from platform identities.
 *
 * A gateway would otherwise "remember who uses which session" — a table, a file, all invalid
 * after one cold start, plus a migration path nobody ever tests. No table here:
 * `telegram:12345` is both this user and that session.
 *
 * The gateway is therefore **stateless**: restart costs nothing, another process doesn't split a
 * person into two talks. To really change sessions, the user taps "restart" and a suffix changes.
 */
export function sessionIdFor(conversation: string): string {
  return conversation;
}

/**
 * Translate what happened in a turn into words for humans.
 *
 * Only the few things really worth interrupting the user for. Successful tool calls are normal —
 * unreported; failures are reported, because the next explanation usually relates to them.
 *
 * Only notifications carrying a recognizable `turnId` appear here — before announcing anything
 * you must know whom to announce to. So `consolidationCompleted` (a batch of facts distilled) is
 * absent: it is sent during a turn but **carries no `turnId`** (see protocol/v2.rs), so with two
 * conversations in flight there's no telling whom to tell. Say less rather than let someone read
 * Alice's memory updates in Bob's chat; bringing it back means adding `turnId` to that notification.
 */
function progressText(notification: ServerNotification): string | null {
  switch (notification.type) {
    case "toolStarted":
      return `在用 ${notification.tool}…`;
    case "toolCompleted":
      return notification.status === "error"
        ? `${notification.tool} 没成功，它换条路继续`
        : null;
    default:
      return null;
  }
}

/**
 * Translate an error into one sentence that can go straight to the user.
 *
 * `JoyError` carries a code precisely for this: **"model not configured" and "feature not built"
 * are two things to a user** — one they fix themselves, the other they wait out. Don't merge them.
 */
export function explain(error: unknown): string {
  if (error instanceof JoyError) {
    if (error.isProvider) {
      return `模型那边没接上：${error.message}\n先去 \`joy config\` 里把 key 和模型名确认一下。`;
    }
    if (error.isNotImplemented) {
      return `这个功能还没做：${error.message}`;
    }
    if (error.isInvalidParams) {
      return `参数不对，多半是网关和 app-server 版本对不上：${error.message}`;
    }
    return `Joy 报错（${error.code}）：${error.message}`;
  }
  return `连不上 Joy：${error instanceof Error ? error.message : String(error)}`;
}

interface InFlight {
  conversation: string;
  /** The derived session id. Also the only clue `turnStarted` gives for "whose turn is this". */
  sessionId: string;
  tools: string[];
  /** This turn's id. Unknowable before `turnStarted`, hence nullable. */
  turnId: string | null;
}

/** A completed reply, waiting to be picked up by its own `turn/start` response. */
interface Completed {
  reply: string;
  model: string;
  iterations: number;
}

/**
 * Which turn a notification belongs to.
 *
 * Almost all notifications carry `turnId`; `error` doesn't — it may be a protocol-level error
 * (parse failure, unknown method), unrelated to any turn.
 */
function turnIdOf(notification: ServerNotification): string | null {
  return "turnId" in notification ? notification.turnId : null;
}

/**
 * One app-server, serving many people.
 *
 * Why not "one process per person": state.db allows a single writer; more processes fight over
 * the lock. So the gateway keeps concurrency outside — one queue per platform user, and queues
 * don't compete either, because the server's stdio loop is serial anyway (see stdio.rs).
 */
export class JoyBridge {
  readonly #client: JoyClient;
  readonly #onProgress: (note: ProgressNote) => void;

  /** One tail per conversation: the next message starts only after the previous one finishes. */
  readonly #tails = new Map<string, Promise<void>>();

  /**
   * Turns in flight, indexed by `turnId`.
   *
   * A single "current turn" slot won't do: the server's stdio loop is serial, but **two turns of
   * different conversations can be in flight at once** — while Alice's runs, Bob's request already
   * went out. Notifications carry only `turnId`; one slot would let Bob's turn claim Alice's.
   */
  readonly #inFlight = new Map<string, InFlight>();

  /** Turns past `turn/start`, awaiting `turnStarted`, by session — turnId still unknown here. */
  readonly #awaitingTurnId = new Map<string, InFlight>();

  /** Completed replies, likewise keyed by `turnId`. */
  readonly #completed = new Map<string, Completed>();

  constructor(options: JoyBridgeOptions = {}) {
    if (options.client !== undefined) {
      this.#client = options.client;
    } else {
      this.#client = new JoyClient({
        transport: new StdioTransport({
          command: options.command,
          args: options.args,
          cwd: options.cwd,
          env: options.env,
        }),
        onLog: options.onLog ?? ((text) => console.error(text)),
      });
    }

    this.#onProgress = options.onProgress ?? (() => {});
    this.#client.onNotification((notification) =>
      this.#handleNotification(notification),
    );
  }

  /**
   * One question in, one reply out.
   *
   * Two calls on the same conversation **always** run back to back — two messages fired quickly,
   * each running a full loop, would stomp context and return out of order. Queuing is the only job.
   */
  ask(conversation: string, text: string): Promise<TurnOutcome> {
    const previous = this.#tails.get(conversation) ?? Promise.resolve();
    const turn = previous.then(() => this.#runTurn(conversation, text));

    // The tail only queues; it carries no errors — a failed turn must not drag the next down.
    const tail = turn.then(
      () => {},
      () => {},
    );
    this.#tails.set(conversation, tail);
    void tail.then(() => {
      // Give the queue slot back once the conversation goes quiet, or it grows forever.
      if (this.#tails.get(conversation) === tail) this.#tails.delete(conversation);
    });

    return turn;
  }

  /** Close app-server. It finishes writing queued frames before exiting. */
  close(): void {
    this.#client.close();
  }

  async #runTurn(conversation: string, text: string): Promise<TurnOutcome> {
    const sessionId = sessionIdFor(conversation);
    const current: InFlight = {
      conversation,
      sessionId,
      tools: [],
      turnId: null,
    };
    this.#awaitingTurnId.set(sessionId, current);

    try {
      // `stream: false`: chat platforms want one complete sentence, not per-character jitter.
      // (`turnCompleted.reply` is exactly that; no need to assemble deltas by hand.)
      const { turnId } = await this.#client.request(METHODS.TURN_START, {
        sessionId,
        message: text,
        stream: false,
      });

      const completed = this.#completed.get(turnId);
      if (completed === undefined) {
        throw new Error(
          `这一轮没有收到 turnCompleted（turnId=${turnId}）—— 服务端的通知和应答对不上`,
        );
      }

      return {
        sessionId,
        reply: completed.reply,
        tools: current.tools,
        model: completed.model,
        iterations: completed.iterations,
      };
    } finally {
      // Clear on success or failure: `run_turn` may fail after `turnStarted` went out (a
      // mid-turn model error), leaving the notification stream broken halfway. The leftovers
      // would be picked up by a later turn, or occupy memory forever.
      this.#forget(current);
    }
  }

  /** Turn wrap-up: erase every trace it left in the tables. */
  #forget(current: InFlight): void {
    this.#awaitingTurnId.delete(current.sessionId);
    if (current.turnId === null) return;
    this.#inFlight.delete(current.turnId);
    this.#completed.delete(current.turnId);
  }

  #handleNotification(notification: ServerNotification): void {
    if (notification.type === "turnStarted") {
      // Only now do we learn this turn's name, so it moves from "waiting by session" to "found by turnId".
      const waiting = this.#awaitingTurnId.get(notification.sessionId);
      if (waiting === undefined) return;
      waiting.turnId = notification.turnId;
      this.#inFlight.set(notification.turnId, waiting);
      return;
    }

    const turnId = turnIdOf(notification);
    if (turnId === null) return;
    const current = this.#inFlight.get(turnId);
    // Unknown turnId: likely a late tail arriving after a stand-in/previous turn ended — drop it.
    if (current === undefined) return;

    const note = progressText(notification);
    if (note !== null) {
      this.#onProgress({ conversation: current.conversation, text: note });
    }

    switch (notification.type) {
      case "toolStarted":
        current.tools.push(notification.tool);
        return;
      case "turnCompleted":
        this.#completed.set(turnId, {
          reply: notification.reply,
          model: notification.meta.model,
          iterations: notification.meta.iterations,
        });
        this.#inFlight.delete(turnId);
        return;
      default:
        return;
    }
  }
}
