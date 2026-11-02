// 把两个静态文件挪进 dist。
//
// tsc 只管 .ts → .js，index.html 和 style.css 得有人搬。为什么不把它们
// 直接放在 dist 里：那样 dist 会混着「生成物」和「手写的源码」，
// .gitignore 得写例外，读的人还得先分清哪些能删哪些不能。

import { copyFile } from "node:fs/promises";

const here = import.meta.dirname; // 脚本在包根，跟 src/ 平级

for (const file of ["index.html", "style.css"]) {
  await copyFile(`${here}/src/${file}`, `${here}/dist/${file}`);
}
