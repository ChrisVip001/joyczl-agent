import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import readline from "node:readline";

/**
 * 一帧的边界。
 *
 * app-server 的 stdout 上只有「一行一条 JSON」，stderr 上是人话（MCP 连不上、
 * 图搭不起来……）。传输层的活因此只有三件：写一行、拆一行、把另外两路原样
 * 交出去。
 *
 * 抽成接口是为了测试能塞一个假的进去 —— 帧的组装/对应关系（id 配对、通知
 * 分发、退出时收敛挂着的请求）是客户端自己的逻辑，不该为了测它去起进程。
 */
export interface Transport {
  write(line: string): void;
  /** 收到一行（不含换行符）。 */
  onLine(handler: (line: string) => void): void;
  /** stderr 上的一行，原样转出，不解析。 */
  onLog(handler: (text: string) => void): void;
  /** 进程退出 / 起不来 / 连接断开。 */
  onClose(handler: (reason: string) => void): void;
  /** 文明关闭：关掉 stdin，让服务端把排着的帧写完再退。 */
  close(): void;
}

/** app-server 就在 `joy` 的这个子命令下。 */
export const APP_SERVER_COMMAND = "app-server";

export interface StdioTransportOptions {
  /** 二进制路径。不给就按 `$JOY_BIN` → 仓库的 target → PATH 找。 */
  command?: string;
  args?: string[];
  cwd?: string;
  env?: NodeJS.ProcessEnv;
}

/**
 * 起一个真的 `joy app-server`，用它的 stdin/stdout 说话。
 *
 * 这是所有客户端的入口形状：一个孩子的两根管子，没有网络、没有端口、
 * 没有要清理的临时目录。服务端死了管子就断，客户端跟着知道。
 */
export class StdioTransport implements Transport {
  readonly #child: ChildProcessWithoutNullStreams;
  readonly #lineHandlers = new Set<(line: string) => void>();
  readonly #logHandlers = new Set<(text: string) => void>();
  readonly #closeHandlers = new Set<(reason: string) => void>();
  #closed = false;

  constructor(options: StdioTransportOptions = {}) {
    const command = options.command ?? findJoyBinary();
    this.#child = spawn(command, options.args ?? [APP_SERVER_COMMAND], {
      cwd: options.cwd,
      env: options.env ?? process.env,
    });

    // 两边都用 readline 拆行：它认得 \n 与 \r\n，也管住「半行」——
    // 手写缓冲拼字符串最后一定会漏掉跨 chunk 的那一刀。
    const stdout = readline.createInterface({ input: this.#child.stdout });
    stdout.on("line", (line) => {
      if (line.trim() === "") return;
      for (const handler of this.#lineHandlers) handler(line);
    });

    const stderr = readline.createInterface({ input: this.#child.stderr });
    stderr.on("line", (line) => {
      if (line.trim() === "") return;
      for (const handler of this.#logHandlers) handler(line);
    });

    this.#child.on("error", (error) => {
      this.#shutdown(`起不来：${error.message}`);
    });
    this.#child.on("close", (code, signal) => {
      this.#shutdown(`退出（code=${code ?? "?"} signal=${signal ?? "?"}）`);
    });
  }

  write(line: string): void {
    if (this.#closed) return;
    this.#child.stdin.write(`${line}\n`);
  }

  onLine(handler: (line: string) => void): void {
    this.#lineHandlers.add(handler);
  }

  onLog(handler: (text: string) => void): void {
    this.#logHandlers.add(handler);
  }

  onClose(handler: (reason: string) => void): void {
    this.#closeHandlers.add(handler);
  }

  close(): void {
    if (this.#closed) return;
    // 只关 stdin，不 kill：服务端看到 stdin 到头就会把通道里剩的帧写完再退
    // （见 stdio.rs 末尾）。这时候补一刀会把最后那几帧吃掉。
    this.#child.stdin.end();
  }

  #shutdown(reason: string): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const handler of this.#closeHandlers) handler(reason);
    this.#lineHandlers.clear();
    this.#logHandlers.clear();
    this.#closeHandlers.clear();
  }

  get closed(): boolean {
    return this.#closed;
  }
}

/**
 * 找 `joy` 二进制。
 *
 * 顺序：`$JOY_BIN` → 从本文件往上找到仓库根的 `joy-rs/target/{debug,release}`
 * → 交给 PATH。
 *
 * 往上走而不是写死「上四级目录」：源码在包里的位置是会动的，而写成固定层数
 * 之后，挪一次目录就会把「找不到二进制」变成一个莫名其妙的 ENOENT。
 */
export function findJoyBinary(): string {
  const override = process.env["JOY_BIN"];
  if (override) return override;

  let dir = path.dirname(fileURLToPath(import.meta.url));
  for (;;) {
    for (const profile of ["debug", "release"]) {
      const candidate = path.join(dir, "joy-rs", "target", profile, "joy");
      if (existsSync(candidate)) return candidate;
    }
    const parent = path.dirname(dir);
    if (parent === dir) return "joy";
    dir = parent;
  }
}
