// Tests for Markdown parsing. Only `parseMarkdown` / `parseInline` — pure functions, no DOM;
// the AST → node half is checked in the browser (build, then open the page).

import assert from "node:assert/strict";
import { test } from "node:test";

import { parseInline, parseMarkdown, stripToolFold } from "../src/markdown.ts";

test("headings, paragraphs and rules each land where they belong", () => {
  const blocks = parseMarkdown("# Verdict\n\nthe first paragraph\nthe second line\n\n---\n");
  assert.deepEqual(
    blocks.map((block) => block.kind),
    ["heading", "paragraph", "rule"],
  );

  const paragraph = blocks[1];
  assert.equal(paragraph?.kind, "paragraph");
  if (paragraph?.kind !== "paragraph") return;
  // Newlines inside a paragraph stay: in a chat a newline is a newline, rendered as <br>.
  assert.equal(paragraph.lines.length, 2);
});

test("** inside a fenced block is kept as it is", () => {
  const blocks = parseMarkdown("```ts\nconst a = **b**;\n```");
  assert.equal(blocks.length, 1);

  const code = blocks[0];
  assert.equal(code?.kind, "code");
  if (code?.kind !== "code") return;
  assert.equal(code.lang, "ts");
  assert.equal(code.text, "const a = **b**;");
});

test("an unclosed fence: the rest counts as code, neither swallowed nor an error", () => {
  const blocks = parseMarkdown("```\nline one\nline two");
  const code = blocks[0];
  assert.equal(code?.kind, "code");
  if (code?.kind !== "code") return;
  assert.equal(code.text, "line one\nline two");
});

test("a more indented line inside a list item becomes a sublist", () => {
  const blocks = parseMarkdown("- one\n  - one-a\n- two");
  const list = blocks[0];
  assert.equal(list?.kind, "list");
  if (list?.kind !== "list") return;

  assert.equal(list.items.length, 2);
  assert.equal(list.items[0]?.length, 2, "the first item holds both text and a sublist");
  assert.equal(list.items[0]?.[1]?.kind, "list");
});

test("an ordered list not starting at 1 keeps its start", () => {
  const blocks = parseMarkdown("3. three\n4. four");
  const list = blocks[0];
  assert.equal(list?.kind, "list");
  if (list?.kind !== "list") return;
  assert.equal(list.ordered, true);
  assert.equal(list.start, 3);
});

test("a GFM table reads back its header, alignment and rows", () => {
  const blocks = parseMarkdown("| item | value |\n| :--- | ---: |\n| a | 1 |\n| b | 2 |");
  const table = blocks[0];
  assert.equal(table?.kind, "table");
  if (table?.kind !== "table") return;

  assert.deepEqual(table.align, ["left", "right"]);
  assert.equal(table.head.length, 2);
  assert.equal(table.rows.length, 2);
});

test("what sits inside a quote is still parsed block by block", () => {
  const blocks = parseMarkdown("> a word of warning\n> a second line");
  const quote = blocks[0];
  assert.equal(quote?.kind, "quote");
  if (quote?.kind !== "quote") return;

  assert.equal(quote.blocks.length, 1);
  assert.equal(quote.blocks[0]?.kind, "paragraph");
});

test("inline markers each become their own node", () => {
  const inlines = parseInline("run `npm test`, **important**, *by the way*, ~~scrapped~~");
  const kinds = inlines.map((inline) => inline.kind);
  assert.ok(kinds.includes("code"));
  assert.ok(kinds.includes("strong"));
  assert.ok(kinds.includes("em"));
  assert.ok(kinds.includes("del"));
});

test("an untrusted link degrades to plain text", () => {
  assert.deepEqual(parseInline("[click me](javascript:alert(1))"), [
    { kind: "text", text: "[click me](javascript:alert(1))" },
  ]);
});

test("an http link keeps its address", () => {
  const inlines = parseInline("[the docs](https://example.com/a)");
  assert.equal(inlines.length, 1);

  const link = inlines[0];
  assert.equal(link?.kind, "link");
  if (link?.kind !== "link") return;
  assert.equal(link.href, "https://example.com/a");
});

test("a multiplication sign and symbols inside a variable name are not emphasis", () => {
  assert.deepEqual(parseInline("2*3*4"), [{ kind: "text", text: "2*3*4" }]);
  assert.deepEqual(parseInline("file_name_here"), [{ kind: "text", text: "file_name_here" }]);
  // No space around the word, and the bold still takes effect.
  assert.equal(parseInline("this is **important**")[1]?.kind, "strong");
});

test("the streaming invariant: whole-line prefixes settle into the same blocks as the full text (what StreamMarkdown rests on)", () => {
  // StreamMarkdown dares to say «a settled block is never redrawn» because of this: Markdown
  // only grows at the tail, so
  // the block sequence of a **whole-line** prefix (minus the last block still growing) is always
  // a prefix of the full text's block sequence.
  // Note this only holds for whole lines — half a line broken inside a `- ` marker parses as a
  // paragraph first; those transients are
  // caught by StreamMarkdown's per-frame reconciliation, not by any property of the parser.
  const docs = [
    "# Title\n\nthe first paragraph with `code` and **bold**.\n\n- alpha\n- beta\n  - beta-a\n\n| item | value |\n| --- | --- |\n| a | 1 |\n\n> a quote\n\n```ts\nconst x = 1;\n```\n\nthe end.",
    "the opening goes straight into a paragraph, no heading, a second line follows.\n\n1. the first step\n2. the second step\n\n---",
    "| only a table | no paragraph before it |\n| :-- | --: |\n| x | y |",
  ];
  for (const doc of docs) {
    const full = parseMarkdown(doc);
    for (let cut = 1; cut <= doc.length; cut += 1) {
      // Cut only at newlines: SSE deltas arrive by word or sentence, so half-line transients
      // exist but are caught by the reconciliation.
      if (cut < doc.length && doc[cut - 1] !== "\n") continue;
      const prefix = parseMarkdown(doc.slice(0, cut));
      const keep = Math.max(0, prefix.length - 1); // the last block is still growing, so skip it
      assert.deepEqual(
        prefix.slice(0, keep),
        full.slice(0, keep),
        `at a prefix of ${cut} characters the settled blocks do not match the full text`,
      );
    }
  }
});

test("stripToolFold: the model-only folded tail is stripped, while what a human wrote or merely mentioned stays", () => {
  const reply = "# Verdict\n\nready to ship.";
  assert.equal(stripToolFold(reply), reply, "a reply with no fold comes back untouched");

  const stored =
    "looked already, there is no report in the directory.\n[tools used: list_dir(.Trash/\n.agent-browser/\n…truncated…); list_dir(Error: Operation not permitted)]";
  assert.equal(
    stripToolFold(stored),
    "looked already, there is no report in the directory.",
    "the tool name, arguments and output (newlines and brackets included) come off whole",
  );

  // When the output quotes the marker itself, lastIndexOf lands on the one really appended.
  const quoted = 'written.\n[tools used: delegate_task(the verdict mentions "[tools used:" which was escaped)]';
  assert.equal(stripToolFold(quoted), "written.");

  // Merely mentioning the marker (not appended) is left alone: no closing bracket, no strip.
  const noClose = "halfway through [tools used: never closed";
  assert.equal(stripToolFold(noClose), noClose);
});
