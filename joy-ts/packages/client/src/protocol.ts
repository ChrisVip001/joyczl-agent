// 全仓库唯一指向生成物的地方。
//
// 别的文件一律从 `./protocol.ts` 引，这样那条又长又丑、还跟目录深度耦合的
// 相对路径只出现一次 —— 挪包的时候只需要改这一个文件。
//
// 生成出来的全是 `export type`，`import type` 在运行时会被 Node 的类型剥离
// 整个抹掉：也就是说这份源码**不依赖生成物在运行时可解析**，只有 tsc 做类型
// 检查时才需要它躺在那个位置。
//
// `CODES` 与 `METHODS` 是两个例外 —— 它们是值，会被真的打进运行时。
// 它们在生成物里是 `.mts`（唯一两个有运行时导出的），所以这里的后缀也是 `.mts`。
export type * from "../../../../joy-rs/joyczl-protocol/schema/typescript/v2/index.ts";
export { CODES } from "../../../../joy-rs/joyczl-protocol/schema/typescript/v2/codes.mts";
export { METHODS } from "../../../../joy-rs/joyczl-protocol/schema/typescript/v2/methods.mts";
