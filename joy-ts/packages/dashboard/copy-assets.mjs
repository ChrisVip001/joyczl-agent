// Move the two static files into dist.
//
// tsc only handles .ts → .js; index.html and style.css need someone to carry them. Why not keep
// them in dist directly: dist would then mix "generated artifacts" with "handwritten sources",
// .gitignore would need exceptions, and readers would have to sort deletable from not.
//
// The last step stamps the bundle URL with this build's id. index.html is network-first
// and main.js is cache-first, so without a stamp a service worker can pair a *new* page
// with an *old* bundle — the page then misses elements the bundle expects and the screen
// goes blank. A query string changes the cache key on every build, so the two halves can
// only ever come from the same one.

import { copyFile, readFile, writeFile } from "node:fs/promises";

const here = import.meta.dirname; // script sits at the package root, beside src/

for (const file of ["index.html", "style.css", "manifest.webmanifest", "sw.js", "icon.svg", "icon-maskable.svg"]) {
  await copyFile(`${here}/src/${file}`, `${here}/dist/${file}`);
}

const build = Date.now().toString(36);
const htmlPath = `${here}/dist/index.html`;
const html = await readFile(htmlPath, "utf8");
const stamped = html.replace('src="./main.js"', `src="./main.js?v=${build}"`);
if (stamped === html) {
  // Fail the build, not the page: an unstamped script tag silently reintroduces the
  // stale-pair bug, and a page that renders wrong is harder to trace than a red build.
  throw new Error('no src="./main.js" in index.html — did the script tag change? the version stamp was not applied');
}
await writeFile(htmlPath, stamped);
