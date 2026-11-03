// The only place in the whole repo pointing at the generated artifacts.
//
// Every other file imports from `./protocol.ts`, so that long, ugly, directory-depth-coupled
// relative path appears once — moving the package means editing exactly this one file.
//
// Everything generated is `export type`, and `import type` is stripped entirely by Node's type
// stripping at runtime: this source **doesn't need the artifacts to be resolvable at runtime**;
// only tsc needs them sitting there for type checking.
//
// `CODES` and `METHODS` are the two exceptions — values, really bundled into the runtime. They
// live in the artifacts as `.mts` (the only two with runtime exports), so the suffix here matches.
export type * from "../../../../joy-rs/joyczl-protocol/schema/typescript/v2/index.ts";
export { CODES } from "../../../../joy-rs/joyczl-protocol/schema/typescript/v2/codes.mts";
export { METHODS } from "../../../../joy-rs/joyczl-protocol/schema/typescript/v2/methods.mts";
