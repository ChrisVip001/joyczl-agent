// The activity timeline: "what the agent is doing", as one quiet vertical thread.
//
// During a turn every event lands here instead of spraying chips across the message —
// gate decisions, tool calls, workflow nodes, retries, consolidation. The thread is
// collapsed to its latest step by default; expanding it shows the whole course.
// On the turn ending, it folds back: the reply is what deserves the attention then.
//
// Interactive pieces (approval cards) do NOT live here — they stay inline in the
// message where a decision can't be buried inside a collapsed list.

import { el, formatMs } from "./render";

/** One step on the thread. */
interface Entry {
  kind: "gate" | "tool" | "graph" | "note" | "bad";
  label: string;
  detail?: string;
}

export interface Timeline {
  /** The element to mount into the message. */
  root: HTMLElement;
  gate(text: string): void;
  retry(text: string): void;
  goal(text: string): void;
  graph(text: string): void;
  note(text: string): void;
  /** A tool call: start opens the entry, end freezes its duration and colors it. */
  toolStart(name: string, detail: string): void;
  toolEnd(name: string, ok: boolean, ms: number | null, output: string): void;
  /** Turn over: fold the thread, freeze the head line. */
  settle(): void;
}

/** The head line shows "N 步 · latest label" — the only thing visible while collapsed. */
function headText(steps: number, latest: string): string {
  return steps <= 1 ? latest : `${steps} 步活动 · ${latest}`;
}

export function createTimeline(): Timeline {
  const entries: Entry[] = [];
  const list = el("div", { class: "tl-body" });
  list.hidden = true;

  const head = el("button", { class: "tl-head", type: "button" });
  const root = el("div", { class: "timeline" }, head, list);

  let open = false;

  head.addEventListener("click", () => {
    // Toggle stays available after settle(): reading back what happened is the point.
    open = !open;
    list.hidden = !open;
    root.classList.toggle("open", open);
  });

  function paintHead(): void {
    const latest = entries[entries.length - 1];
    head.textContent = latest === undefined ? "" : headText(entries.length, latest.label);
  }

  function push(entry: Entry): HTMLElement {
    entries.push(entry);
    const row = tlRow(entry);
    list.append(row);
    while (list.children.length > 50) list.firstElementChild?.remove();
    paintHead();
    return row;
  }

  /** One row: kind-colored dot + label (+ detail). Pure paint, no timers here. */
  function tlRow(entry: Entry): HTMLElement {
    return el(
      "div",
      { class: `tl-row tl-${entry.kind}` },
      el("span", { class: "tl-dot", "aria-hidden": "true" }),
      el("span", { class: "tl-label", text: entry.label }),
      entry.detail === undefined || entry.detail === ""
        ? el("span")
        : el("span", { class: "tl-detail", text: entry.detail, title: entry.detail }),
    );
  }

  let toolName: string | null = null;
  let toolRow: HTMLElement | null = null;

  return {
    root,

    gate: (text) => push({ kind: "gate", label: text }),
    retry: (text) => push({ kind: "note", label: text }),
    goal: (text) => push({ kind: "gate", label: text }),
    graph: (text) => push({ kind: "graph", label: text }),
    note: (text) => push({ kind: "note", label: text }),

    toolStart(name, detail) {
      toolName = name;
      const row = push({ kind: "tool", label: name, detail });
      row.classList.add("running");
      toolRow = row;
    },

    toolEnd(name, ok, ms, output) {
      if (toolRow !== null && toolName === name) {
        toolRow.classList.remove("running");
        toolRow.classList.add(ok ? "done-ok" : "done-bad");
        const detail = toolRow.querySelector(".tl-detail");
        if (ms !== null && detail !== null) {
          detail.textContent = formatMs(ms);
        }
        if (!ok && output !== "") {
          const label = toolRow.querySelector<HTMLElement>(".tl-label");
          if (label !== null) label.title = output;
        }
      }
      toolRow = null;
      toolName = null;
      paintHead();
    },

    settle() {
      open = false;
      list.hidden = true;
      root.classList.remove("open");
      root.classList.add("ended");
      paintHead();
    },
  };
}
