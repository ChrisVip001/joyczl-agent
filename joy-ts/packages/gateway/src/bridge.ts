import { JoyClient, JoyError, METHODS, StdioTransport } from "@joy/client";
import type { ServerNotification } from "@joy/client";

/** 一轮说完之后留下来的东西。 */
export interface TurnOutcome {
  /** Joy 那边的会话 id —— 就是 `conversation` 本身，见 `sessionIdFor`。 */
  sessionId: string;
  reply: string;
  /** 这轮实际用了哪些工具，按调用顺序，重复的算多次。 */
  tools: string[];
  /** 真正答话的模型。被 graph 判成 quick 时那是小模型 —— 如实带出来。 */
  model: string;
  iterations: number;
}

/** 一轮进行中的动静。适配器可以拿它去显示「正在输入…」。 */
export interface ProgressNote {
  conversation: string;
  text: string;
}

/**
 * 适配器跟 Joy 之间就这一个接口：来一条消息，回一句话。
 *
 * 收成一个函数而不是直接吃 `JoyBridge`，是为了适配器能独立被测 ——
 * 长轮询、WebSocket 心跳、切长消息、白名单这些跟 Joy 一点关系都没有。
 *
 * 它跟 `ProgressNote` 一样属于**平台无关**的那一半，所以放在这儿，
 * 而不是某个平台的适配器里 —— 否则 Discord 得从 Telegram 那个文件里
 * 导入自己的接口。
 */
export type Responder = (conversation: string, text: string) => Promise<string>;

export interface JoyBridgeOptions {
  /** `joy` 二进制路径。不传就按客户端那套顺序找。 */
  command?: string;
  args?: string[];
  cwd?: string;
  /** 透给 app-server 的环境变量（`JOY_HOME`、各家 API key……）。 */
  env?: NodeJS.ProcessEnv;
  /**
   * 已经连好的客户端。不给就自己起一个 app-server。
   *
   * 这个缝是留给测试的：排队、通知归拢、失败后清残骸这几件事全是纯逻辑，
   * 不该为了测它们去真的起一个进程、配一个模型、等一次网络。
   */
  client?: JoyClient;
  /** 服务端的人话与本地杂音。默认打到 stderr。 */
  onLog?: (text: string) => void;
  /** 一轮里的进度。默认丢掉。 */
  onProgress?: (note: ProgressNote) => void;
}

/**
 * 会话 id 从平台身份直接派出来。
 *
 * 网关本来是要「记住谁在用哪个会话」的 —— 一张表、一个文件、一次冷启动之后
 * 全部失效，外加一条永远没人测的迁移路径。这里不要那张表：
 * `telegram:12345` 既是这位用户，也是那个会话。
 *
 * 于是网关是**无状态**的：重启的代价是零，多开一个进程也不会把同一个人
 * 劈成两段对话。真要换会话，让用户点一下「重新开始」，换个后缀就是。
 */
export function sessionIdFor(conversation: string): string {
  return conversation;
}

/**
 * 把一轮里发生的事翻译成给人看的话。
 *
 * 只挑真的值得打扰用户的那几件。工具调用成功是常态，不用报；失败要报，
 * 因为下一句解释通常就跟它有关。
 *
 * 这里只有认得出 `turnId` 的通知 —— 播报之前得先知道该播给谁。
 * 所以 `consolidationCompleted`（提炼完一批事实）不在其中：它确实是在一轮里
 * 发的，但**不带 `turnId`**（见 protocol/v2.rs），两段对话同时在飞时无从判断
 * 该报给谁。宁可少说一句，也不要让人在甲的聊天框里读到乙的记忆动态。
 * 想让这句回来，得先给那个通知补上 `turnId`。
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
 * 把错误翻成一句能直接发给用户的话。
 *
 * `JoyError` 带 code 就是为了这里：**「模型没配好」和「这功能还没做」对用户
 * 是两件事**，前者他能自己解决，后者只能等。混成一句「出错了」等于让他白折腾。
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
  /** 派生出来的会话 id。也是 `turnStarted` 反查「这一轮是谁的」的唯一线索。 */
  sessionId: string;
  tools: string[];
  /** 这一轮的 id。`turnStarted` 之前无从得知，所以是可空的。 */
  turnId: string | null;
}

/** 收好的一轮答复，等着被自己的 `turn/start` 应答取走。 */
interface Completed {
  reply: string;
  model: string;
  iterations: number;
}

/**
 * 一条通知说的是哪一轮。
 *
 * 绝大多数通知都带 `turnId`；`error` 不带 —— 它可能是协议层面的错误
 * （解析失败、方法不存在），跟某一轮没关系。
 */
function turnIdOf(notification: ServerNotification): string | null {
  return "turnId" in notification ? notification.turnId : null;
}

/**
 * 一个 app-server，服务很多人。
 *
 * 为什么不是「一个人一个进程」：state.db 只允许一个进程写，多开就是抢锁。
 * 所以网关这边把并发挡在门外 —— 每个平台用户一条队列，队列之间也不用抢，
 * 因为服务端那条 stdio 循环本来就是一件一件做的（见 stdio.rs）。
 */
export class JoyBridge {
  readonly #client: JoyClient;
  readonly #onProgress: (note: ProgressNote) => void;

  /** 每段对话一条尾巴：上一条消息处理完了，下一条才开始。 */
  readonly #tails = new Map<string, Promise<void>>();

  /**
   * 在飞的轮次，按 `turnId` 索引。
   *
   * 不能只留「当前这一轮」这一个槽位：服务端的 stdio 循环虽然一件一件地做，
   * 但**不同对话的两轮可以同时在飞** —— 甲那轮还在跑的时候，乙的请求已经写出去了。
   * 通知只带 `turnId` 不带会话，单个槽位会让乙的轮次把甲的通知认成自己的。
   */
  readonly #inFlight = new Map<string, InFlight>();

  /** 已发出 `turn/start`、还没等到 `turnStarted` 的轮次，按会话索引 —— 这会儿还不知道 turnId。 */
  readonly #awaitingTurnId = new Map<string, InFlight>();

  /** 收好的答复，同样按 `turnId`。 */
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
   * 问一句，拿回一句。
   *
   * 同一段对话上的两次调用**一定**一前一后 —— 用户连发两条消息时，两条各自
   * 跑一轮完整 loop 会互相踩上下文，回来还会颠倒顺序。排队是这里唯一负责的事。
   */
  ask(conversation: string, text: string): Promise<TurnOutcome> {
    const previous = this.#tails.get(conversation) ?? Promise.resolve();
    const turn = previous.then(() => this.#runTurn(conversation, text));

    // 尾巴只用来排队，不承载错误 —— 上一轮失败了不该把下一轮也拖下去。
    const tail = turn.then(
      () => {},
      () => {},
    );
    this.#tails.set(conversation, tail);
    void tail.then(() => {
      // 对话不聊了就把队列格子还回去，不然它会一直长。
      if (this.#tails.get(conversation) === tail) this.#tails.delete(conversation);
    });

    return turn;
  }

  /** 关掉 app-server。它会先把排着的帧写完再退。 */
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
      // `stream: false`：聊天平台要的是一句完整的话，不是逐字抖动。
      // （`turnCompleted.reply` 就是它，不用自己拿 delta 拼。）
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
      // 成了还是败了都要清：`run_turn` 可能在发完 `turnStarted` 之后才失败
      // （模型中途报错之类），那时通知串断在半路。留着的半截会被后来的一轮
      // 捡到，或者永远占着内存。
      this.#forget(current);
    }
  }

  /** 一轮收场：把它留在几张表里的痕迹全抹掉。 */
  #forget(current: InFlight): void {
    this.#awaitingTurnId.delete(current.sessionId);
    if (current.turnId === null) return;
    this.#inFlight.delete(current.turnId);
    this.#completed.delete(current.turnId);
  }

  #handleNotification(notification: ServerNotification): void {
    if (notification.type === "turnStarted") {
      // 到这一刻才知道这一轮叫什么名字，于是它从「按会话等」挪进「按 turnId 找」。
      const waiting = this.#awaitingTurnId.get(notification.sessionId);
      if (waiting === undefined) return;
      waiting.turnId = notification.turnId;
      this.#inFlight.set(notification.turnId, waiting);
      return;
    }

    const turnId = turnIdOf(notification);
    if (turnId === null) return;
    const current = this.#inFlight.get(turnId);
    // 不认识这个 turnId：可能是替身/上一轮收场之后姗姗来迟的尾巴，丢掉。
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
