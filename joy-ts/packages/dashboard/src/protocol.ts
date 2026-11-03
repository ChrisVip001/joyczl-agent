// The only place in the repo pointing at the generated artifacts (browser side). Same path as
// `packages/client/src/protocol.ts` — that long, ugly relative path still appears only once.
//
// Everything generated is `export type`: type-checked fully, erased entirely at bundle time,
// leaving not a byte in the produced JS — this source **doesn't need the artifacts at runtime**.
//
// `CODES` / `METHODS` aren't re-exported (the artifacts' only two values): the dashboard speaks
// HTTP, not JSON-RPC; the translation happens on the Rust side (`joyczl-ops`).
export type * from "../../../../joy-rs/joyczl-protocol/schema/typescript/v2/index.ts";
