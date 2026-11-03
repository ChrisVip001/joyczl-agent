/**
 * The little bit of text processing shared by chat platforms.
 *
 * One thing only: split a long reply into pieces the platform accepts. Telegram is 4096 chars,
 * Discord is 2000 — the numbers differ, the splitting is identical, so it's written once and
 * the limit is passed in by each adapter.
 */

/**
 * Split a long reply into pieces.
 *
 * A sentence hard-cut mid-way looks far worse than one cut at a newline, so prefer a newline —
 * but only accept one past the halfway mark, otherwise a reply that opens with a newline gets
 * shredded into fragments.
 */
export function splitMessage(text: string, limit: number): string[] {
  if (text.length <= limit) return [text];

  const chunks: string[] = [];
  let rest = text;
  while (rest.length > limit) {
    const window = rest.slice(0, limit);
    const newline = window.lastIndexOf("\n");
    const cut = newline > limit / 2 ? newline : limit;
    chunks.push(rest.slice(0, cut));
    rest = rest.slice(cut).replace(/^\n/, "");
  }
  if (rest !== "") chunks.push(rest);
  return chunks;
}
