import { CODES, METHODS } from "./protocol.ts";
import type {
  ApprovalRespondResponse,
  ApprovalRespondParams,
  ConfigReadParams,
  ConfigReadResponse,
  ConfigWriteParams,
  ConfigWriteResponse,
  ErrorObject,
  JsonRpcError,
  JsonRpcMessage,
  JsonRpcNotification,
  MemoryForgetParams,
  MemoryForgetResponse,
  MemoryListParams,
  MemoryListResponse,
  MemoryRememberParams,
  MemoryRememberResponse,
  MemorySearchParams,
  MemorySearchResponse,
  ModelListParams,
  ModelListResponse,
  ServerNotification,
  SessionListParams,
  SessionListResponse,
  SessionNewParams,
  SessionNewResponse,
  TurnInterruptParams,
  TurnInterruptResponse,
  TurnStartParams,
  TurnStartResponse,
} from "./protocol.ts";
import type { Transport } from "./transport.ts";

/**
 * 「哪个方法收什么、回什么」的对照表。
 *
 * 这里手写的**只有配对关系**，形状全部来自生成物 —— Rust 侧的方法名是个
 * 运行时字符串常量，跟 `*Params` 类型之间没有任何类型层面的联系，生成器
 * 无从推导，只能在这边写一次。
 *
 * 写不错的地方：
 * - 方法名用生成的计算属性名（`METHODS.*`），Rust 改名这里就编译不过；
 * - 参数/应答直接引用生成的类型，字段改名这里跟着变。
 * 剩下唯一能漂的就是「配错对」—— 配错了 tsc 会在调用处报出来，因为形状不合。
 */
export interface Methods {
  [METHODS.TURN_START]: { params: TurnStartParams; result: TurnStartResponse };
  [METHODS.TURN_INTERRUPT]: {
    params: TurnInterruptParams;
    result: TurnInterruptResponse;
  };
  [METHODS.APPROVAL_RESPOND]: {
    params: ApprovalRespondParams;
    result: ApprovalRespondResponse;
  };
  [METHODS.SESSION_LIST]: {
    params: SessionListParams;
    result: SessionListResponse;
  };
  [METHODS.SESSION_NEW]: { params: SessionNewParams; result: SessionNewResponse };
  [METHODS.MEMORY_SEARCH]: {
    params: MemorySearchParams;
    result: MemorySearchResponse;
  };
  [METHODS.MEMORY_LIST]: { params: MemoryListParams; result: MemoryListResponse };
  [METHODS.MEMORY_REMEMBER]: {
    params: MemoryRememberParams;
    result: MemoryRememberResponse;
  };
  [METHODS.MEMORY_FORGET]: {
    params: MemoryForgetParams;
    result: MemoryForgetResponse;
  };
  [METHODS.CONFIG_READ]: { params: ConfigReadParams; result: ConfigReadResponse };
  [METHODS.CONFIG_WRITE]: {
    params: ConfigWriteParams;
    result: ConfigWriteResponse;
  };
  [METHODS.MODEL_LIST]: { params: ModelListParams; result: ModelListResponse };
}

export type MethodName = keyof Methods;

/**
 * 服务端回的错，按 JSON-RPC 的错误对象还原。
 *
 * 带上 `code` 而不是只留一句话，是因为调用方**真的会按它分流**：
 * `PROVIDER_ERROR` 要提示用户去配 key，`NOT_IMPLEMENTED` 是「这功能还没做」
 * 而不是「你调错了」，两者对用户是两件事（见 rpc.rs 里那段注释）。
 */
export class JoyError extends Error {
  readonly code: number;
  readonly data: unknown;

  constructor(object: ErrorObject) {
    super(object.message);
    this.name = "JoyError";
    this.code = object.code;
    this.data = object.data ?? null;
  }

  /** key 无效、限流、模型不存在…… 都是这一类。 */
  get isProvider(): boolean {
    return this.code === CODES.PROVIDER_ERROR;
  }

  /** 协议里有、当前阶段还没做。 */
  get isNotImplemented(): boolean {
    return this.code === CODES.NOT_IMPLEMENTED;
  }

  /** 参数形状不对 —— 多半是本端跟服务端版本对不上。 */
  get isInvalidParams(): boolean {
    return this.code === CODES.INVALID_PARAMS;
  }
}

/** 通知在线上统一挂在这个方法名底下，真正的判别式在 `params.type`。 */
const NOTIFICATION_METHOD = "turn/notification";

export interface JoyClientOptions {
  transport: Transport;
  /**
   * 服务端的人话（stderr）与解析不了的帧。默认打到 `console.error`：
   * 这些是「该被看见但不该中断流程」的东西，吞掉才是错的。
   */
  onLog?: (text: string) => void;
}

interface Pending {
  resolve: (result: unknown) => void;
  reject: (error: Error) => void;
  method: string;
}

/**
 * app-server 的客户端。
 *
 * 只做三件事，多一件都不做：
 * 1. 给请求配 id、按 id 把应答还给出请求的那个 `await`；
 * 2. 把通知原样分发给订阅者 —— **不解释、不缓存、不重放**。一轮对话的
 *    过程事件属于当下那个 UI，客户端替它记着只会记出另一份状态。
 * 3. 进程一没，把所有挂着的请求一口气拒掉。不然调用方会永远等下去，
 *    而「永远等下去」是最难查的那种坏法。
 */
export class JoyClient {
  readonly #transport: Transport;
  readonly #onLog: (text: string) => void;
  /** key 是 id 归一化后的字符串 —— 见 `asId`。 */
  readonly #pending = new Map<string, Pending>();
  readonly #notificationHandlers = new Set<
    (notification: ServerNotification) => void
  >();
  #nextId = 1;
  #closedReason: string | null = null;

  constructor(options: JoyClientOptions) {
    this.#transport = options.transport;
    this.#onLog = options.onLog ?? ((text) => console.error(text));

    this.#transport.onLine((line) => this.#handleLine(line));
    this.#transport.onLog((text) => this.#onLog(text));
    this.#transport.onClose((reason) => this.#handleClose(reason));
  }

  /**
   * 调一个方法，等它的应答。
   *
   * 参数是必填的 —— 连「什么都不带」也要写成 `{}`。可选的第二参数会让
   * 「忘了传」和「就传空」看起来一样，而这个协议里它们往往不一样。
   */
  request<M extends MethodName>(
    method: M,
    params: Methods[M]["params"],
  ): Promise<Methods[M]["result"]> {
    if (this.#closedReason !== null) {
      return Promise.reject(new Error(`连接已经没了：${this.#closedReason}`));
    }

    const id = this.#nextId;
    this.#nextId += 1;

    return new Promise<Methods[M]["result"]>((resolve, reject) => {
      this.#pending.set(asId(id), {
        resolve: resolve as (result: unknown) => void,
        reject,
        method,
      });
      this.#transport.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    });
  }

  /** 订阅过程事件。返回退订函数。 */
  onNotification(handler: (notification: ServerNotification) => void): () => void {
    this.#notificationHandlers.add(handler);
    return () => {
      this.#notificationHandlers.delete(handler);
    };
  }

  /** 文明关闭：不再收新请求，等排着的帧写完。 */
  close(): void {
    if (this.#closedReason !== null) return;
    this.#closedReason = "客户端主动关闭";
    this.#transport.close();
    this.#rejectAll(this.#closedReason);
  }

  #handleLine(line: string): void {
    let frame: JsonRpcMessage | JsonRpcNotification;
    try {
      frame = JSON.parse(line) as JsonRpcMessage | JsonRpcNotification;
    } catch {
      // 服务端只会写合法 JSON；真写坏了说明它自己也出了事，
      // 但这一句坏掉不该让客户端跟着崩 —— 抖出去给人看。
      this.#onLog(`这一帧读不出来，原样贴出来：${line}`);
      return;
    }

    if ("method" in frame) {
      this.#handleNotification(frame);
      return;
    }

    if ("error" in frame) {
      this.#handleError(frame);
      return;
    }

    const key = asId(frame.id);
    const pending = this.#pending.get(key);
    if (pending === undefined) {
      this.#onLog(`应答的 id 没人在等：${JSON.stringify(frame.id)}`);
      return;
    }
    this.#pending.delete(key);
    pending.resolve(frame.result);
  }

  #handleNotification(frame: JsonRpcNotification): void {
    if (frame.method !== NOTIFICATION_METHOD) {
      this.#onLog(`不认识的通知方法：${frame.method}`);
      return;
    }
    // `params` 在生成物里是 `unknown`（服务端那一侧它就是一段 JSON）。
    // 这里**刻意不做运行时校验**：协议的形状由 Rust 侧的类型保证，
    // 在客户端再抄一遍校验逻辑就是又一份会漂的合同。真出了形状不对的帧，
    // 那是服务端的 bug，前端会在用到那个字段时才炸 —— 而那时离现场更近。
    const notification = frame.params as ServerNotification | null | undefined;
    if (notification == null || typeof notification !== "object") {
      this.#onLog(`通知没有内容：${JSON.stringify(frame)}`);
      return;
    }
    for (const handler of this.#notificationHandlers) handler(notification);
  }

  #handleError(frame: JsonRpcError): void {
    // 解析不了请求时服务端给不了 id（JSON-RPC 规定为 null）——没有请求
    // 能拿这个错，只能当噪音。
    if (frame.id === null) {
      this.#onLog(frame.error.message);
      return;
    }

    const id = asId(frame.id);
    const pending = this.#pending.get(id);
    if (pending === undefined) {
      this.#onLog(`错误的 id 没人在等：${frame.error.message}`);
      return;
    }
    this.#pending.delete(id);
    pending.reject(new JoyError(frame.error));
  }

  #handleClose(reason: string): void {
    if (this.#closedReason === null) this.#closedReason = reason;
    this.#rejectAll(reason);
  }

  #rejectAll(reason: string): void {
    const pending = [...this.#pending.values()];
    this.#pending.clear();
    for (const entry of pending) {
      // 把方法名带上：一个永远挂着的 await 最难查的地方就是不知道
      // 它在等哪个请求。
      entry.reject(new Error(`${entry.method} 没等到应答，连接断了：${reason}`));
    }
  }
}

/**
 * 帧里的 id 归一化成 Map 的 key。
 *
 * JSON-RPC 的 id 可以是数字也可以是字符串（`RequestId` 就是这么定义的），
 * 两种都得能用同一条路配对回来 —— 所以按字符串存，不猜类型。
 *
 * 存和取**必须**都过这里：`Map` 不帮你转换 `1` 和 `"1"`，键写成数字、
 * 查用字符串，配不上是静悄悄的 —— 那个 `await` 会永远挂着。
 */
function asId(id: number | string): string {
  return String(id);
}
