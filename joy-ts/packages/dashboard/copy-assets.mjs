// Move the two static files into dist.
//
// tsc only handles .ts → .js; index.html and style.css need someone to carry them. Why not keep
// them in dist directly: dist would then mix "generated artifacts" with "handwritten sources",
// .gitignore would need exceptions, and readers would have to sort deletable from not.

import { copyFile } from "node:fs/promises";

const here = import.meta.dirname; // script sits at the package root, beside src/

for (const file of ["index.html", "style.css"]) {
  await copyFile(`${here}/src/${file}`, `${here}/dist/${file}`);
}
