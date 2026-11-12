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
  kind: "gate" | "tool" | "graph" | "note";
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
  /**
   * A tool call: start opens the entry, end freezes its duration and colors it.
   *
   * Keyed by the model's call id, not the name: several read-only calls share one round and
   * can start and finish in any order, so "the last row" would put results on the wrong line.
   */
  toolStart(id: string, name: string, detail: string): void;
  /** Live output from a call that is still running: the row's detail shows the tail. */
  toolDelta(id: string, chunk: string): void;
  toolEnd(id: string, name: string, ok: boolean, ms: number | null, output: string): void;
  /** Turn over: fold the thread, freeze the head line. */
  settle(): void;
}

/** The head line shows "N 步 · latest label" — the only thing visible while collapsed. */
function headText(steps: number, latest: string): string {
  return steps <= 1 ? latest : `${steps} 步活动 · ${latest}`;
}

export function createTimeline(): Timeline {
  // The head line needs only "how many steps + the latest label" — the DOM keeps the rest.
  let steps = 0;
  let latestLabel = "";
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
    head.textContent = steps === 0 ? "" : headText(steps, latestLabel);
  }

  function push(entry: Entry): HTMLElement {
    steps += 1;
    latestLabel = entry.label;
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
      el("span", {
        class: "tl-detail",
        text: entry.detail ?? "",
        title: entry.detail ?? "",
      }),
    );
  }

  interface OpenRow {
    id: string;
    row: HTMLElement;
    detail: HTMLElement;
  }

  /** Open rows by call id. Bounded like the list itself: a long turn must not leak. */
  const toolRows = new Map<string, OpenRow>();
  /** The last row opened without an id — the fallback for an unkeyed completion. */
  let lastToolRow: OpenRow | null = null;

  function detailOf(row: HTMLElement): HTMLElement {
    return row.querySelector<HTMLElement>(".tl-detail") ?? el("span", { class: "tl-detail" });
  }

  function finish(row: HTMLElement, detail: HTMLElement, ok: boolean, ms: number | null, output: string): void {
    row.classList.remove("running");
    row.classList.add(ok ? "done-ok" : "done-bad");
    if (ms !== null) detail.textContent = formatMs(ms);
    // Failures keep the full text one hover away; live output is only ever a tail.
    if (!ok && output !== "") {
      const label = row.querySelector<HTMLElement>(".tl-label");
      if (label !== null) label.title = output;
    }
  }

  return {
    root,

    gate: (text) => push({ kind: "gate", label: text }),
    retry: (text) => push({ kind: "note", label: text }),
    goal: (text) => push({ kind: "gate", label: text }),
    graph: (text) => push({ kind: "graph", label: text }),
    note: (text) => push({ kind: "note", label: text }),

    toolStart(id, name, detail) {
      const row = push({ kind: "tool", label: name, detail });
      row.classList.add("running");
      const entry: OpenRow = { id, row, detail: detailOf(row) };
      if (id !== "") toolRows.set(id, entry);
      lastToolRow = entry;
    },

    toolDelta(id, chunk) {
      const entry = toolRows.get(id);
      if (entry === undefined || chunk === "") return;
      // The tail, not the whole log: this is one line in a collapsed thread. The full output
      // still arrives on toolCompleted, which is what a human opens when they care.
      const tail = (entry.detail.textContent ?? "") + chunk;
      const shown = tail.length > 240 ? `…${tail.slice(-240)}` : tail;
      entry.detail.textContent = shown.replace(/\s+$/, "");
      entry.detail.title = shown;
      entry.row.classList.add("streaming");
    },

    toolEnd(id, name, ok, ms, output) {
      const entry = toolRows.get(id) ?? (lastToolRow?.id === id || id === "" ? lastToolRow : null);
      if (entry !== null) {
        finish(entry.row, entry.detail, ok, ms, output);
        toolRows.delete(entry.id);
        if (lastToolRow === entry) lastToolRow = null;
      } else if (name !== "") {
        // An unkeyed completion with no open row (a client that joined late): a fence post is
        // still better than silence — the model did call something.
        const row = push({ kind: "tool", label: name });
        finish(row, detailOf(row), ok, ms, output);
      }
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
