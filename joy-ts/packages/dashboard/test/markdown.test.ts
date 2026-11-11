// Markdown 解析的测试。只测 `parseMarkdown` / `parseInline` —— 纯函数，不碰 DOM；
// AST → 节点的部分交给浏览器里看（构建完直接开页面）。

import assert from "node:assert/strict";
import { test } from "node:test";

import { parseInline, parseMarkdown } from "../src/markdown.ts";

test("标题、段落、分隔线各归各位", () => {
  const blocks = parseMarkdown("# 结论\n\n第一段\n第二行\n\n---\n");
  assert.deepEqual(
    blocks.map((block) => block.kind),
    ["heading", "paragraph", "rule"],
  );

  const paragraph = blocks[1];
  assert.equal(paragraph?.kind, "paragraph");
  if (paragraph?.kind !== "paragraph") return;
  // 段落里的换行要留着：聊天里换行就是换行，渲染时变 <br>。
  assert.equal(paragraph.lines.length, 2);
});

test("围栏代码里的 ** 原样保留", () => {
  const blocks = parseMarkdown("```ts\nconst a = **b**;\n```");
  assert.equal(blocks.length, 1);

  const code = blocks[0];
  assert.equal(code?.kind, "code");
  if (code?.kind !== "code") return;
  assert.equal(code.lang, "ts");
  assert.equal(code.text, "const a = **b**;");
});

test("没写收尾的围栏：剩下的都算代码，不吞掉也不报错", () => {
  const blocks = parseMarkdown("```\nline one\nline two");
  const code = blocks[0];
  assert.equal(code?.kind, "code");
  if (code?.kind !== "code") return;
  assert.equal(code.text, "line one\nline two");
});

test("列表项里缩进更深的行成为子列表", () => {
  const blocks = parseMarkdown("- 一\n  - 一甲\n- 二");
  const list = blocks[0];
  assert.equal(list?.kind, "list");
  if (list?.kind !== "list") return;

  assert.equal(list.items.length, 2);
  assert.equal(list.items[0]?.length, 2, "第一项里既有文字也有子列表");
  assert.equal(list.items[0]?.[1]?.kind, "list");
});

test("有序列表不是从 1 开始时保留起点", () => {
  const blocks = parseMarkdown("3. 三\n4. 四");
  const list = blocks[0];
  assert.equal(list?.kind, "list");
  if (list?.kind !== "list") return;
  assert.equal(list.ordered, true);
  assert.equal(list.start, 3);
});

test("GFM 表格读出表头、对齐和数据行", () => {
  const blocks = parseMarkdown("| 项 | 值 |\n| :--- | ---: |\n| a | 1 |\n| b | 2 |");
  const table = blocks[0];
  assert.equal(table?.kind, "table");
  if (table?.kind !== "table") return;

  assert.deepEqual(table.align, ["left", "right"]);
  assert.equal(table.head.length, 2);
  assert.equal(table.rows.length, 2);
});

test("引用里的内容继续按块解析", () => {
  const blocks = parseMarkdown("> 一句提醒\n> 第二行");
  const quote = blocks[0];
  assert.equal(quote?.kind, "quote");
  if (quote?.kind !== "quote") return;

  assert.equal(quote.blocks.length, 1);
  assert.equal(quote.blocks[0]?.kind, "paragraph");
});

test("行内标记各自成节点", () => {
  const inlines = parseInline("用 `npm test` 跑，**很重要**，*顺便*，~~作废~~");
  const kinds = inlines.map((inline) => inline.kind);
  assert.ok(kinds.includes("code"));
  assert.ok(kinds.includes("strong"));
  assert.ok(kinds.includes("em"));
  assert.ok(kinds.includes("del"));
});

test("不可信的链接降级成原文", () => {
  assert.deepEqual(parseInline("[点我](javascript:alert(1))"), [
    { kind: "text", text: "[点我](javascript:alert(1))" },
  ]);
});

test("http 链接保留地址", () => {
  const inlines = parseInline("[文档](https://example.com/a)");
  assert.equal(inlines.length, 1);

  const link = inlines[0];
  assert.equal(link?.kind, "link");
  if (link?.kind !== "link") return;
  assert.equal(link.href, "https://example.com/a");
});

test("乘号和变量名里的符号不当强调", () => {
  assert.deepEqual(parseInline("2*3*4"), [{ kind: "text", text: "2*3*4" }]);
  assert.deepEqual(parseInline("file_name_here"), [{ kind: "text", text: "file_name_here" }]);
  // 中文两侧不写空格，粗体照常生效。
  assert.equal(parseInline("这很**重要**")[1]?.kind, "strong");
});
