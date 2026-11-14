import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import readline from "node:readline";

/**
 * Frame boundaries.
 *
 * app-server's stdout carries "one JSON per line"; stderr carries human words (MCP unreachable,
 * graph unbuildable…). The transport's job is only three things: write a line, split a line,
 * pass the other stream through untouched.
 *
 * The interface exists so tests can slot in a fake — frame assembly/correlation (id pairing,
 * dispatch, settling pending requests on exit) is the client's logic, not something to spawn for.
 */
export interface Transport {
  write(line: string): void;
  /** One line received (newline excluded). */
  onLine(handler: (line: string) => void): void;
  /** A line from stderr, passed through as-is, unparsed. */
  onLog(handler: (text: string) => void): void;
  /** Process exited / failed to start / connection dropped. */
  onClose(handler: (reason: string) => void): void;
  /** Civilized close: close stdin, let the server finish writing queued frames, then exit. */
  close(): void;
}

/** app-server lives under this subcommand of `joy`. */
export const APP_SERVER_COMMAND = "app-server";

export interface StdioTransportOptions {
  /** Binary path. If absent, search `$JOY_BIN` → repo target → PATH. */
  command?: string;
  args?: string[];
  cwd?: string;
  env?: NodeJS.ProcessEnv;
}

/**
 * Start a real `joy app-server` and speak over its stdin/stdout.
 *
 * This is the entry shape for all clients: one child, two pipes, no network, no port, no temp
 * directory to clean up. When the server dies the pipes break, and the client knows with it.
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

    // Both sides split lines with readline: it knows \n and \r\n and handles "half lines" —
    // a hand-rolled buffer will eventually miss the cut across chunks.
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
      this.#shutdown(`cannot start: ${error.message}`);
    });
    this.#child.on("close", (code, signal) => {
      this.#shutdown(`exited (code=${code ?? "?"} signal=${signal ?? "?"})`);
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
    // Close stdin only, no kill: seeing EOF on stdin, the server finishes writing remaining
    // frames and exits (see the end of stdio.rs). Killing now would swallow the last few frames.
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
 * Find the `joy` binary.
 *
 * Order: `$JOY_BIN` → walk up from this file to the repo root's `joy-rs/target/{debug,release}`
 * → hand over to PATH.
 *
 * Walk up instead of hardcoding "four levels up": the source's location inside the package moves,
 * and a fixed depth turns every move into a baffling ENOENT — "binary not found" with no hint.
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
