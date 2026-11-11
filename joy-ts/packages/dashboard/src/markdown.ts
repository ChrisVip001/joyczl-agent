// 把模型的 Markdown 画成人能一眼看懂的样子。
//
// 为什么手写：页面本身没有任何运行时依赖，为了渲染一段回复去引一个 parser + sanitizer，
// 会成为这一页最大的依赖。这里的做法是 **直接建 DOM 节点** —— `innerHTML` 依然一次都不用
// （见 render.ts 的 `el`），所以模型输出永远只能是文本：文字是文字，只有下面这些结构才会
// 变成元素。
//
// 支持：ATX 标题、段落、围栏代码、引用、有序/无序列表（可嵌套）、GFM 表格、分隔线；
// 行内：代码、粗体、斜体、删除线、链接。
// 故意不支持：原始 HTML（按文本显示）、图片（回复不该自己发起远程请求）、引用式链接、脚注。

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

/** Markdown 源文 → 块序列。纯函数，不碰 DOM（测试只测这一层）。 */
export function parseMarkdown(source: string): Block[] {
  return parseBlocks(source.replace(/\r\n?/g, "\n").split("\n"));
}

/** Markdown 源文 → 可直接 append 的节点。 */
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
        // 开头是同样长度的围栏就算收尾；没写收尾时，整段剩余内容都是代码。
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

/** 同级列表项收集成一个块；更深缩进的行归到当前项里，递归时自然成为子列表。 */
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

    // 项与项之间允许一个空行，但只有下一行仍是同级项时才继续。
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

/** 行内标记 → 片段序列。 */
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

    // 地址里可能带一层括号（`…/A_(b)`、`alert(1)`），所以允许 `(...)` 夹在中间。
    const link = /^\[([^\]]*)\]\(((?:[^()\s]+|\([^()\s]*\))+)(?:\s+"[^"]*")?\)/.exec(rest);
    if (link !== null) {
      flush();
      const href = safeHref(link[2] ?? "");
      const label = link[1] ?? "";
      if (href === null) {
        // 不可信的 scheme 降级成原文：链接文字照常显示，但不再是可点的地址。
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
 * `2*3*4`、`file_name_here` 里的星号/下划线是符号，不是强调。
 * 判断依据：一对标记的外侧都是 ASCII 字母数字 → 不当作强调（中文两侧没有空格也照常生效）。
 */
function emphasisAllowed(text: string, at: number, matched: string): boolean {
  const before = at === 0 ? " " : text[at - 1] ?? " ";
  const after = text[at + matched.length] ?? " ";
  return !(/[0-9A-Za-z]/.test(before) && /[0-9A-Za-z]/.test(after));
}

/** 只放行能安全跳转的地址；`javascript:` 之类连锚点都不生成。 */
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

/** 表格一行 → 单元格。首尾的竖线不算列。 */
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
        // 项里只有一个段落时不再套 <p> —— 列表要紧凑，段落间距留给正文。
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
 * 剥掉落库助手消息里模型专用的工具折叠尾巴。
 *
 * 历史（chat_log）写给两个读者。**模型**需要 `fold_tool_activity`（app-server 的
 * turn.rs）追加的 `\n[tools used: …]` —— 没有它下一轮就忘了自己做过什么；
 * **人**不需要：气泡上方的 trace 胶囊已经说了用过哪些工具，原始工具输出在对话里
 * 只是噪音（「我的家目录列表怎么跑到聊天里了？」）。所以展示层把它摘掉 —— 一份
 * 存储文本，两种画法，折叠留在数据库里。
 *
 * 标记是机器追加在**最末尾**的，所以：取**最后一次**出现（工具输出可能引用这个
 * 标记），并且只在文本真的以折叠的收尾括号结束时才剥 —— 回复里只是提到标记的
 * 原样保留。
 */
export function stripToolFold(content: string): string {
  const at = content.lastIndexOf("\n[tools used:");
  if (at === -1 || !content.endsWith("]")) return content;
  return content.slice(0, at);
}

/**
 * 流式渲染：回复一边到一边就画成 Markdown，不再等传输完。
 *
 * 增量的粒度是**块**：解析用同一个 `parseMarkdown`，但每个动画帧只画有变化的部分
 * —— 落定的块各留一个节点，每帧只重画仍在生长的最后一块，几千 token 的回复也
 * 不会每个 delta 都整体重排。断在半截的行内标记（`**`、`` ` ``）下一帧自然长好。
 *
 * 解析器对「半行」前缀没有严格的前缀稳定性：流断在 `- ` 标记中间时，孤立的 `-`
 * 先被解析成段落，下一帧才并回列表。所以每次重画都拿落定块和最新解析**对账**
 * （缓存的序列化签名），从第一处不一致的块起作废重画 —— 正常情况下全等，代价
 * 是每帧一次 O(回复长度) 的比较，和解析同阶。
 *
 * 节点直接画进宿主元素（不套中间层），`.md > :first-child` 这类子选择器照常生效。
 * 流结束时调一次 `finish()`，把仍在生长的最后一块画完。
 */
export class StreamMarkdown {
  private source = "";
  /** 落定块下标 → 节点。画过就不再碰——除非对账发现它变了。 */
  private settled: HTMLElement[] = [];
  /** 落定块的序列化签名，对账用。与 `settled` 一一对应。 */
  private signatures: string[] = [];
  /** 仍在生长的最后一块；每个动画帧换一个新节点。 */
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

  /** 流结束：取消排队的帧，把剩余内容一次画完。之后这个实例不再接收 push。 */
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
    // 流式时最后一块还在长（live）；final 时没有活块，全部按落定处理。
    const live = final ? -1 : blocks.length - 1;
    this.tail?.remove();
    this.tail = null;

    // 对账：找到第一处与缓存不一致的落定块（正常情况全部一致，at = settled.length）。
    let at = 0;
    while (
      at < this.settled.length &&
      at < blocks.length &&
      this.signatures[at] === JSON.stringify(blocks[at] as Block)
    ) {
      at += 1;
    }
    if (at < this.settled.length) {
      // 解析器 reinterpret 了早先的尾部（半行的 `- ` 并回列表等）：作废重画。
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

/** 段落内的单换行渲染成 <br>：聊天里换行就是换行，不并成一行。 */
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
        // 外部地址另开一页：点一下不该把对话顶掉。
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        appendInline(link, inline.children);
        parent.appendChild(link);
        break;
      }
    }
  }
}
