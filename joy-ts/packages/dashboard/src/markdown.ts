// Renders the model's Markdown so a human can read it at a glance.
//
// Why hand-written: the page has no runtime dependencies, and pulling in a parser plus a
// sanitiser just to render one reply would become this page's biggest dependency. The
// approach here is to **build DOM nodes directly** — `innerHTML` is never used (see `el` in
// render.ts) — so model output can only ever be text: words stay words, and only the
// structures below become elements.
//
// Supported: ATX headings, paragraphs, fenced code, blockquotes, ordered and unordered lists
// (nestable), GFM tables, horizontal rules; inline: code, bold, italic, strikethrough, links.
// Deliberately unsupported: raw HTML (shown as text), images (a reply must not trigger remote
// requests on its own), reference links, footnotes.

export type Inline =
  | { kind: "text"; text: string }
  | { kind: "code"; text: string }
  | { kind: "strong"; children: Inline[] }
  | { kind: "em"; children: Inline[] }
  | { kind: "del"; children: Inline[] }
  | { kind: "link"; href: string; children: Inline[] };

type Align = "left" | "center" | "right" | null;

export type Block =
  | { kind: "heading"; level: number; inlines: Inline[] }
  | { kind: "paragraph"; lines: Inline[][] }
  | { kind: "code"; lang: string; text: string }
  | { kind: "quote"; blocks: Block[] }
  | { kind: "list"; ordered: boolean; start: number; items: Block[][] }
  | { kind: "table"; align: Align[]; head: Inline[][]; rows: Inline[][][] }
  | { kind: "rule" };

const FENCE = /^\s{0,3}(`{3,}|~{3,})\s*([^\s`]*)\s*$/;
const HEADING = /^\s{0,3}(#{1,6})\s+(.*)$/;
const RULE = /^\s{0,3}(?:(?:\*\s*){3,}|(?:-\s*){3,}|(?:_\s*){3,})$/;
const QUOTE = /^\s{0,3}>\s?/;
const LIST_ITEM = /^(\s*)([-*+]|\d{1,9}[.)])\s+(.*)$/;
const ALIGN_ROW = /^\s{0,3}\|?\s*:?-{1,}:?\s*(?:\|\s*:?-{1,}:?\s*)*\|?\s*$/;

/** Markdown source → a block sequence. A pure function that never touches the DOM (the tests
 *  only cover this layer). */
export function parseMarkdown(source: string): Block[] {
  return parseBlocks(source.replace(/\r\n?/g, "\n").split("\n"));
}

/** Markdown source → nodes that can be appended as they are. */
export function renderMarkdown(source: string): DocumentFragment {
  const fragment = document.createDocumentFragment();
  for (const block of parseMarkdown(source)) fragment.append(renderBlock(block));
  return fragment;
}

function parseBlocks(lines: string[]): Block[] {
  const blocks: Block[] = [];
  let at = 0;

  while (at < lines.length) {
    const line = lines[at] ?? "";
    if (isBlank(line)) {
      at += 1;
      continue;
    }

    const fence = FENCE.exec(line);
    if (fence !== null) {
      const marker = fence[1] ?? "```";
      const lang = (fence[2] ?? "").trim();
      const body: string[] = [];
      at += 1;
      while (at < lines.length) {
        const current = lines[at] ?? "";
        // A closing fence of the same length ends it; with no closing fence, everything left is code.
        if (current.trimStart().startsWith(marker)) {
          at += 1;
          break;
        }
        body.push(current);
        at += 1;
      }
      blocks.push({ kind: "code", lang, text: body.join("\n") });
      continue;
    }

    const heading = HEADING.exec(line);
    if (heading !== null) {
      blocks.push({
        kind: "heading",
        level: (heading[1] ?? "#").length,
        inlines: parseInline((heading[2] ?? "").trim()),
      });
      at += 1;
      continue;
    }

    if (RULE.test(line)) {
      blocks.push({ kind: "rule" });
      at += 1;
      continue;
    }

    if (QUOTE.test(line)) {
      const inner: string[] = [];
      while (at < lines.length) {
        const current = lines[at] ?? "";
        if (!QUOTE.test(current)) break;
        inner.push(current.replace(QUOTE, ""));
        at += 1;
      }
      blocks.push({ kind: "quote", blocks: parseBlocks(inner) });
      continue;
    }

    if (line.includes("|") && isAlignRow(lines[at + 1])) {
      const head = splitRow(line);
      const align = alignOf(lines[at + 1] ?? "");
      at += 2;
      const rows: Inline[][][] = [];
      while (at < lines.length) {
        const current = lines[at] ?? "";
        if (isBlank(current) || !current.includes("|")) break;
        rows.push(splitRow(current).map(parseInline));
        at += 1;
      }
      blocks.push({ kind: "table", align, head: head.map(parseInline), rows });
      continue;
    }

    const item = LIST_ITEM.exec(line);
    if (item !== null) {
      const parsed = parseList(lines, at);
      blocks.push(parsed.block);
      at = parsed.next;
      continue;
    }

    const paragraph: string[] = [line.trim()];
    at += 1;
    while (at < lines.length) {
      const current = lines[at] ?? "";
      if (isBlank(current) || startsBlock(current)) break;
      paragraph.push(current.trim());
      at += 1;
    }
    blocks.push({ kind: "paragraph", lines: paragraph.map(parseInline) });
  }

  return blocks;
}

/** Sibling list items collect into one block; deeper-indented lines join the current item and
 *  become a sublist once the recursion gets there. */
function parseList(lines: string[], from: number): { block: Block; next: number } {
  const first = LIST_ITEM.exec(lines[from] ?? "");
  const indent = indentOf(lines[from] ?? "");
  const ordered = /\d/.test(first?.[2] ?? "");
  const start = ordered ? Number.parseInt(first?.[2] ?? "1", 10) || 1 : 1;
  const items: Block[][] = [];
  let at = from;

  while (at < lines.length) {
    const hit = LIST_ITEM.exec(lines[at] ?? "");
    if (hit === null) break;
    if (indentOf(lines[at] ?? "") !== indent) break;
    if (/\d/.test(hit[2] ?? "") !== ordered) break;

    const body: string[] = [hit[3] ?? ""];
    at += 1;
    while (at < lines.length) {
      const current = lines[at] ?? "";
      if (isBlank(current)) break;
      if (indentOf(current) <= indent) break;
      body.push(stripIndent(current, indent + 2));
      at += 1;
    }
    items.push(parseBlocks(body));

    // One blank line between items is allowed, but only when the next line is still a sibling item.
    let skipped = false;
    while (at < lines.length && isBlank(lines[at] ?? "")) {
      const peek = lines[at + 1] ?? "";
      const peekHit = LIST_ITEM.exec(peek);
      if (
        peekHit !== null &&
        indentOf(peek) === indent &&
        /\d/.test(peekHit[2] ?? "") === ordered &&
        !skipped
      ) {
        at += 1;
        skipped = true;
        continue;
      }
      break;
    }
  }

  return { block: { kind: "list", ordered, start, items }, next: at };
}

/** Inline markup → a sequence of fragments. */
export function parseInline(text: string): Inline[] {
  const out: Inline[] = [];
  let buffer = "";
  let at = 0;

  const flush = (): void => {
    if (buffer !== "") {
      out.push({ kind: "text", text: buffer });
      buffer = "";
    }
  };

  while (at < text.length) {
    const rest = text.slice(at);

    const code = /^`([^`\n]+)`/.exec(rest);
    if (code !== null) {
      flush();
      out.push({ kind: "code", text: code[1] ?? "" });
      at += code[0].length;
      continue;
    }

    const strong = /^(\*\*|__)(?=\S)([\s\S]+?)(?<=\S)\1/.exec(rest);
    if (strong !== null && emphasisAllowed(text, at, strong[0])) {
      flush();
      out.push({ kind: "strong", children: parseInline(strong[2] ?? "") });
      at += strong[0].length;
      continue;
    }

    const del = /^~~(?=\S)([\s\S]+?)(?<=\S)~~/.exec(rest);
    if (del !== null) {
      flush();
      out.push({ kind: "del", children: parseInline(del[1] ?? "") });
      at += del[0].length;
      continue;
    }

    // An address may carry one level of parentheses (`…/A_(b)`, `alert(1)`), so `(...)` is
            // allowed in the middle.
    const link = /^\[([^\]]*)\]\(((?:[^()\s]+|\([^()\s]*\))+)(?:\s+"[^"]*")?\)/.exec(rest);
    if (link !== null) {
      flush();
      const href = safeHref(link[2] ?? "");
      const label = link[1] ?? "";
      if (href === null) {
        // An untrusted scheme degrades to plain text: the link text still shows, but it is no
                // longer a clickable address.
        out.push({ kind: "text", text: link[0] });
      } else {
        out.push({
          kind: "link",
          href,
          children: label === "" ? [{ kind: "text", text: href }] : parseInline(label),
        });
      }
      at += link[0].length;
      continue;
    }

    const em = /^(\*|_)(?=\S)([\s\S]+?)(?<=\S)\1/.exec(rest);
    if (em !== null && !(em[2] ?? "").includes(em[1] ?? "") && emphasisAllowed(text, at, em[0])) {
      flush();
      out.push({ kind: "em", children: parseInline(em[2] ?? "") });
      at += em[0].length;
      continue;
    }

    buffer += text[at] ?? "";
    at += 1;
  }

  flush();
  return out;
}

/**
  * The asterisk or underscore in `2*3*4` or `file_name_here` is a symbol, not emphasis.
  * The rule: when both sides of a marker pair are ASCII alphanumerics, it is not emphasis
  * (Chinese text with no spaces on either side still works).
 */
function emphasisAllowed(text: string, at: number, matched: string): boolean {
  const before = at === 0 ? " " : text[at - 1] ?? " ";
  const after = text[at + matched.length] ?? " ";
  return !(/[0-9A-Za-z]/.test(before) && /[0-9A-Za-z]/.test(after));
}

/** Only addresses that are safe to navigate to are let through; something like `javascript:`
 *  does not even produce an anchor. */
function safeHref(raw: string): string | null {
  const href = raw.trim();
  if (/^(?:https?:|mailto:)/i.test(href)) return href;
  if (href.startsWith("/") || href.startsWith("#")) return href;
  return null;
}

function isAlignRow(line: string | undefined): boolean {
  if (line === undefined) return false;
  const trimmed = line.trim();
  if (!trimmed.includes("-")) return false;
  return ALIGN_ROW.test(trimmed);
}

function alignOf(line: string): Align[] {
  return splitRow(line).map((cell) => {
    const left = cell.startsWith(":");
    const right = cell.endsWith(":") && cell.length > 1;
    if (left && right) return "center";
    if (right) return "right";
    if (left) return "left";
    return null;
  });
}

/** One table row → its cells. The leading and trailing pipes are not columns. */
function splitRow(line: string): string[] {
  const trimmed = line.trim().replace(/^\|/, "").replace(/\|$/, "");
  return trimmed.split("|").map((cell) => cell.trim());
}

function startsBlock(line: string): boolean {
  return (
    FENCE.test(line) ||
    HEADING.test(line) ||
    RULE.test(line) ||
    QUOTE.test(line) ||
    LIST_ITEM.test(line)
  );
}

function isBlank(line: string): boolean {
  return line.trim() === "";
}

function indentOf(line: string): number {
  const expanded = line.replace(/\t/g, "    ");
  return expanded.length - expanded.trimStart().length;
}

function stripIndent(line: string, width: number): string {
  const expanded = line.replace(/\t/g, "    ");
  let cut = 0;
  while (cut < width && expanded[cut] === " ") cut += 1;
  return expanded.slice(cut);
}

// --- AST → DOM ----------------------------------------------------------------

function node(tag: string, className?: string): HTMLElement {
  const element = document.createElement(tag);
  if (className !== undefined) element.className = className;
  return element;
}

function renderBlock(block: Block): HTMLElement {
  switch (block.kind) {
    case "rule":
      return node("hr");

    case "heading": {
      const level = Math.min(6, Math.max(1, block.level));
      const element = node(`h${level}`);
      appendInline(element, block.inlines);
      return element;
    }

    case "paragraph": {
      const element = node("p");
      appendLines(element, block.lines);
      return element;
    }

    case "code": {
      const pre = node("pre");
      const code = node("code");
      if (block.lang !== "") code.classList.add(`lang-${block.lang}`);
      code.textContent = block.text;
      pre.append(code);
      return pre;
    }

    case "quote": {
      const quote = node("blockquote");
      for (const inner of block.blocks) quote.append(renderBlock(inner));
      return quote;
    }

    case "list": {
      const element = node(block.ordered ? "ol" : "ul");
      if (block.ordered && block.start !== 1) element.setAttribute("start", String(block.start));
      for (const item of block.items) {
        const li = node("li");
        // A single paragraph inside an item does not get another <p> — lists stay tight, and the
                // paragraph spacing is left to body text.
        const only = item.length === 1 ? item[0] : undefined;
        if (only !== undefined && only.kind === "paragraph") {
          appendLines(li, only.lines);
        } else {
          for (const child of item) li.append(renderBlock(child));
        }
        element.append(li);
      }
      return element;
    }

    case "table": {
      const wrapper = node("div", "md-table");
      const table = node("table");
      const thead = node("thead");
      const headRow = node("tr");
      block.head.forEach((cell, index) => {
        const th = node("th");
        const align = block.align[index] ?? null;
        if (align !== null) th.style.textAlign = align;
        appendInline(th, cell);
        headRow.append(th);
      });
      thead.append(headRow);
      table.append(thead);

      if (block.rows.length > 0) {
        const tbody = node("tbody");
        for (const row of block.rows) {
          const tr = node("tr");
          row.forEach((cell, index) => {
            const td = node("td");
            const align = block.align[index] ?? null;
            if (align !== null) td.style.textAlign = align;
            appendInline(td, cell);
            tr.append(td);
          });
          tbody.append(tr);
        }
        table.append(tbody);
      }

      wrapper.append(table);
      return wrapper;
    }
  }
}

/**
  * Strips the model-only tool-fold tail from a stored assistant message.
  *
  * The history (chat_log) is written for two readers. **The model** needs the `\n[tools used: …]`
  * that `fold_tool_activity` (app-server's turn.rs) appends — without it the next round forgets
  * what it did; **a human** does not: the trace capsule above the bubble already says which
  * tools ran, and the raw tool output in the conversation is just noise ("why is my home
  * directory listing in the chat?"). So the display layer takes it out — one stored text, two
  * renderings, the fold stays in the database.
  *
  * The marker is appended at the **very end**, so: take the **last** occurrence (tool output may
  * quote the marker), and only strip when the text really ends with the folded closing bracket —
  * a reply that merely mentions the marker is left alone.
 *
  * The history (chat_log) is written for two readers. **The model** needs the `\n[tools used: …]`
  * that `fold_tool_activity` (app-server's turn.rs) appends — without it the next round forgets
  * what it did; **a human** does not: the trace capsule above the bubble already says which
  * tools ran, and the raw tool output in the conversation is just noise ("why is my home
  * directory listing in the chat?"). So the display layer takes it out — one stored text, two
  * renderings, the fold stays in the database.
 *
  * The marker is appended at the **very end**, so: take the **last** occurrence (tool output may
  * quote the marker), and only strip when the text really ends with the folded closing bracket —
  * a reply that merely mentions the marker is left alone.
 */
export function stripToolFold(content: string): string {
  const at = content.lastIndexOf("\n[tools used:");
  if (at === -1 || !content.endsWith("]")) return content;
  return content.slice(0, at);
}

/**
 * Streaming: the reply is drawn as Markdown as it arrives, instead of waiting for the whole
 * transfer.
 *
 * The increment is the **block**: parsing goes through the same `parseMarkdown`, but each
 * animation frame only draws what changed — every settled block keeps its node, and each frame
 * redraws only the last block that is still growing, so a reply of thousands of tokens does not
 * get fully re-laid-out per delta. Inline markers cut in half (`**`, `` ` ``) grow back the next
 * frame.
 *
 * The parser has no strict prefix stability for half-lines: when the stream breaks in the middle
 * of a `- ` marker, the lone `-` parses as a paragraph first and only joins the list the next
 * frame. So every redraw **reconciles** the settled blocks against a fresh parse (cached
 * serialised signatures) and invalidates from the first block that differs — normally all equal,
 * at the cost of one O(reply length) comparison per frame, the same order as parsing.
 *
 * Nodes are drawn straight into the host element (no intermediate layer), so child selectors like
 * `.md > :first-child` keep working. At the end of the stream `finish()` is called once to finish
 * the last block that is still growing.
 */
export class StreamMarkdown {
  private source = "";
  /** Settled block index → node. Once drawn it is not touched again — unless reconciliation
   *  finds that it changed. */
  private settled: HTMLElement[] = [];
  /** Serialised signature of the settled blocks, used for reconciliation. One-to-one with
   *  `settled`. */
  private signatures: string[] = [];
  /** The last block that is still growing; every animation frame gets a new node. */
  private tail: HTMLElement | null = null;
  private frame: number | null = null;
  private readonly host: HTMLElement;

  constructor(host: HTMLElement) {
    this.host = host;
  }

  push(delta: string): void {
    this.source += delta;
    this.schedule();
  }

  /** End of stream: cancel the queued frame and draw what is left in one go. After this the
   *  instance takes no more pushes. */
  finish(): void {
    if (this.frame !== null) {
      cancelAnimationFrame(this.frame);
      this.frame = null;
    }
    this.repaint(true);
  }

  private schedule(): void {
    if (this.frame !== null) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = null;
      this.repaint(false);
    });
  }

  private repaint(final: boolean): void {
    const blocks = parseMarkdown(this.source);
        // While streaming the last block is still growing (live); at the end there is no live
        // block and everything is treated as settled.
    const live = final ? -1 : blocks.length - 1;
    this.tail?.remove();
    this.tail = null;

        // Reconcile: find the first settled block that differs from the cache (normally all of them
        // match, at = settled.length).
    let at = 0;
    while (
      at < this.settled.length &&
      at < blocks.length &&
      this.signatures[at] === JSON.stringify(blocks[at] as Block)
    ) {
      at += 1;
    }
    if (at < this.settled.length) {
            // The parser reinterpreted the earlier tail (a half-line `- ` joining a list, and so on):
            // invalidate and redraw.
      for (const stale of this.settled.slice(at)) stale.remove();
      this.settled.length = at;
      this.signatures.length = at;
    }
    for (let i = at; i < blocks.length && i !== live; i += 1) {
      const block = blocks[i] as Block;
      const element = renderBlock(block);
      this.settled.push(element);
      this.signatures.push(JSON.stringify(block));
      this.host.append(element);
    }
    if (live >= 0) {
      this.tail = renderBlock(blocks[live] as Block);
      this.host.append(this.tail);
    }
  }
}

/** A single newline inside a paragraph renders as <br>: in a chat, a line break is a line
 *  break, not a space. */
function appendLines(parent: Node, lines: Inline[][]): void {
  lines.forEach((line, index) => {
    if (index > 0) parent.appendChild(document.createElement("br"));
    appendInline(parent, line);
  });
}

function appendInline(parent: Node, inlines: Inline[]): void {
  for (const inline of inlines) {
    switch (inline.kind) {
      case "text":
        parent.appendChild(document.createTextNode(inline.text));
        break;

      case "code": {
        const code = node("code");
        code.textContent = inline.text;
        parent.appendChild(code);
        break;
      }

      case "strong": {
        const strong = node("strong");
        appendInline(strong, inline.children);
        parent.appendChild(strong);
        break;
      }

      case "em": {
        const em = node("em");
        appendInline(em, inline.children);
        parent.appendChild(em);
        break;
      }

      case "del": {
        const del = node("del");
        appendInline(del, inline.children);
        parent.appendChild(del);
        break;
      }

      case "link": {
        const link = document.createElement("a");
        link.href = inline.href;
                // An external address opens a new page: one click must not displace the conversation.
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        appendInline(link, inline.children);
        parent.appendChild(link);
        break;
      }
    }
  }
}
