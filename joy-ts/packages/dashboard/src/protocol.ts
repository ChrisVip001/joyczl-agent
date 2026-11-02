// 全仓库唯一指向生成物的地方（浏览器这一侧）。跟 `packages/client/src/protocol.ts`
// 写的是同一个路径 —— 那又长又丑、还跟目录深度耦合的相对路径仍然只出现一次。
//
// 生成出来的全是 `export type`：类型检查时它们是全部，打包时被整个抹掉，
// 产出的 JS 里一个字节都不剩。也就是说这份源码**不依赖生成物在运行时可解析**。
//
// 这里不导出 `CODES` / `METHODS`（生成物里唯一两个值）：驾驶舱不亲自发
// JSON-RPC，它说的是 HTTP，翻译是在 Rust 那边（`joyczl-ops`）做的。
export type * from "../../../../joy-rs/joyczl-protocol/schema/typescript/v2/index.ts";
