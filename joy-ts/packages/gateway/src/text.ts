/**
 * 聊天平台共用的那点文本处理。
 *
 * 只有一件事：把一段长回答切成平台能收的几条。Telegram 是 4096 字符、
 * Discord 是 2000 —— 数字不同，切法一模一样，所以切法只写一份，
 * 上限由各自的适配器传进来。
 */

/**
 * 把长回答切成几段。
 *
 * 一段话被硬切在中间比切在换行处难看得多，所以能凑到换行就凑 ——
 * 但只在半程以后才认那个换行，不然一个开头就换行的回答会被切成
 * 一堆碎渣。
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
