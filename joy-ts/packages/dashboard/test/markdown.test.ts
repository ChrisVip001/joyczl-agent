// Markdown 解析的测试。只测 `parseMarkdown` / `parseInline` —— 纯函数，不碰 DOM；
// AST → 节点的部分交给浏览器里看（构建完直接开页面）。

import assert from "node:assert/strict";
import { test } from "node:test";

import { parseInline, parseMarkdown, stripToolFold } from "../src/markdown.ts";

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

test("流式不变量：整行前缀解析出的落定块与全文一致（StreamMarkdown 的根基）", () => {
  // StreamMarkdown 敢「落定块不再重画」，靠的是这条：Markdown 只从尾部生长，
  // **整行**前缀的块序列（去掉仍在生长的最后一块）永远是全文块序列的前缀。
  // 注意这只对整行成立 —— 断在 `- ` 标记中间的半行会先被解析成段落，那类瞬态
  // 由 StreamMarkdown 的逐帧对账兜住，不是解析器的性质。
  const docs = [
    "# 标题\n\n第一段带 `code` 和 **粗体**。\n\n- 甲\n- 乙\n  - 乙一\n\n| 项 | 值 |\n| --- | --- |\n| a | 1 |\n\n> 引用\n\n```ts\nconst x = 1;\n```\n\n结尾。",
    "开场白直接进段落，没有标题，第二行接在后面。\n\n1. 第一步\n2. 第二步\n\n---",
    "| 只有一个表 | 没有前面的段落 |\n| :-- | --: |\n| x | y |",
  ];
  for (const doc of docs) {
    const full = parseMarkdown(doc);
    for (let cut = 1; cut <= doc.length; cut += 1) {
      // 只在换行处切：SSE 的 delta 通常按词/句到，半行瞬态存在但由对账兜底。
      if (cut < doc.length && doc[cut - 1] !== "\n") continue;
      const prefix = parseMarkdown(doc.slice(0, cut));
      const keep = Math.max(0, prefix.length - 1); // 最后一块还在长，不比
      assert.deepEqual(
        prefix.slice(0, keep),
        full.slice(0, keep),
        `前缀截到 ${cut} 字符时，落定块和全文不一致`,
      );
    }
  }
});

test("stripToolFold：模型专用的折叠尾巴剥掉，人写的和只是提到的保留", () => {
  const reply = "# 结论\n\n可以直接上线。";
  assert.equal(stripToolFold(reply), reply, "没有折叠痕迹的回复原样返回");

  const stored =
    "已经看过了，目录里没有报告。\n[tools used: list_dir(.Trash/\n.agent-browser/\n…截断…); list_dir(Error: Operation not permitted)]";
  assert.equal(
    stripToolFold(stored),
    "已经看过了，目录里没有报告。",
    "工具名、参数、输出（含换行和括号）整体剥掉",
  );

  // 输出里引用了标记本身时，lastIndexOf 落在真正追加的那一条上。
  const quoted = '写好了。\n[tools used: delegate_task(结论里提到 "[tools used:" 已被转义)]';
  assert.equal(stripToolFold(quoted), "写好了。");

  // 只是提到标记（没有被追加）的不动：没有收尾括号就不剥。
  const noClose = "说到一半 [tools used: 还没收尾";
  assert.equal(stripToolFold(noClose), noClose);
});
