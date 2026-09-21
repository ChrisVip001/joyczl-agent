# ① 协议层：`joyczl-protocol`

这一层只干一件事：**让「Joy 与客户端之间的契约」只有一个定义处**。定义在 Rust
类型上，TS 类型、JSON Schema、Python 模型都由它生成，漂移由 CI 挡住。

目录：`joy-rs/joyczl-protocol/`（含 `joyczl-protocol-noop-macros/` 与 `schema/`）。

## 1.1 两个子模块，两件不同的事

```
src/lib.rs          pub use protocol::v2::*;  pub use rpc::*;
src/protocol/mod.rs pub mod v2;
src/protocol/v2.rs  业务契约：13 个方法常量 + 63 个类型
src/rpc.rs          JSON-RPC 2.0 信封（与业务无关的通用层）
src/export.rs       生成器：TS / JSON Schema / 常量
src/export_tests.rs 生成入口（标了 #[ignore]，只在需要时跑）
```

划分的理由写在 `v2.rs` 头部：**信封固定**（JSON-RPC 规范说什么就是什么），
**业务类型随版本走**（v1 是搬迁期只读兼容层，新东西一律进 v2）。两者放一个文件
会让人分不清「改这个字段会不会破坏 JSON-RPC 兼容性」。

## 1.2 命名与 wire 约定（改协议前必读）

`v2.rs:7-16` 定了几条硬规矩：

- 类型后缀固定：请求参数 `*Params`、应答 `*Response`、推送 `*Notification`。
- wire 一律 camelCase（`#[serde(rename_all = "camelCase")]` + 同款 `#[ts]`）。
- **整数字段一律 `i32`，不用 `i64`**。原因很具体：`i64` 会被 ts-rs 映射成 TS 的
  `bigint`，而 `JSON.parse` 得到的是 `number`，写起来不报错、跑起来不符。
  存储层是 i64，跨层时用 `narrow()` 夹到 `i32`（`app-server/lib.rs`）。

## 1.3 63 个类型都在哪

唯一清单是 `export.rs:34-110` 的 `protocol_types!` 宏（一个宏列出全部类型名，
既做导出、也做 JSON Schema 平铺）。分类：

| 类别 | 数量 | 例子 |
|---|---|---|
| JSON-RPC 信封（`rpc.rs`） | 7 | `RequestId`、`JsonRpcRequest`、`JsonRpcError`、`JsonRpcMessage` |
| 通用 | 8 | `TokenUsage`、`GateDecision`、`GraphInfo`、`TurnMeta` |
| 会话历史 | 2 | `MessageRole`、`Message` |
| 记忆 | 2 | `Fact`、`Episode` |
| 会话/模型/配置 | 4 | `SessionSummary`、`ModelInfo`、`SettingsView`、`SettingsPatch` |
| 请求/应答对 | 26 | 13 组 `*Params` / `*Response` |
| 驾驶舱 | 1 | `DashboardData` |
| 通知 | 13 | 12 个 `*Notification` + `ServerNotification` 枚举 |

**13 个方法**（`v2.rs:23-37`，值形如 `<资源>/<动作>`）：

```
turn/start  turn/interrupt
session/list  session/new  session/messages
memory/search  memory/list  memory/list-episodes  memory/remember  memory/forget
config/read  config/write
model/list
```

`ServerNotification` 是 `#[serde(tag = "type", rename_all = "camelCase")]` 的枚举
（12 个变体），所以线上一帧通知长这样：

```json
{"jsonrpc":"2.0","method":"turn/notification",
 "params":{"type":"textDelta","turnId":"t…","delta":"你好"}}
```

## 1.4 信封层 `rpc.rs` 的三个细节

- `RequestId` 是 `#[serde(untagged)] enum { Number(i32), String(String) }` —— 
  于是「数字 id」与「字符串 id」都能解析（有些客户端用字符串）。它同时 derive
  `Hash`，因为服务端要拿它当 `HashMap` 的 key 配对 pending。
- `JsonRpcError.id: Option<RequestId>`：解析都失败了，id 无从得知，JSON-RPC 规定
  此处为 `null`。
- `codes` 常量表分两段：标准段（`-32700` 解析、`-32600` 请求非法、`-32601` 方法
  不存在、`-32602` 参数非法、`-32603` 内部错）与 Joy 自定段
  （`-32000` provider 错、`-32001` 工具错、`-32002` **还没实现**）。
  最后这个刻意与 `METHOD_NOT_FOUND` 分开：「还没做」和「你拼错了」是两件事。

## 1.5 单一来源生成管线（本项目最值得抄的一段）

**问题**：Rust 是权威，但 TS 与 Python 也要类型。三种语言各写一份必然漂移。

**做法**：Rust 类型上加 derive，生成物**提交进仓库**，CI 跑一次生成并比对。

```
#[derive(Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TokenUsage { pub input_tokens: i32, pub output_tokens: i32 }
```

**关键技巧：非 test 构建下这两个 derive 是空操作。** `lib.rs:26-33` 按 `cfg` 切换：

```rust
#[cfg(test)]        pub(crate) use schemars::JsonSchema;
#[cfg(test)]        pub(crate) use ts_rs::TS;
#[cfg(not(test))]   pub(crate) use joyczl_protocol_noop_macros::{JsonSchema, TS};
```

`joyczl-protocol-noop-macros` 是 30 行的 proc-macro crate，两个 derive 直接返回空
`TokenStream`。它必须**声明同一组 helper attribute**（`ts` / `schemars` / `serde`），
否则类型上的 `#[ts(export_to = "v2/")]` 会在线上构建里变成「未知属性」编译错误。

于是：正式二进制不依赖 `ts-rs` / `schemars`，也不付 derive 的编译成本；
只有 `cargo test` 构建里生成器才存在。

**生成物**（`joyczl-protocol/schema/`，全部提交）：

| 路径 | 内容 |
|---|---|
| `typescript/v2/*.ts` | 63 个类型，一类型一文件 |
| `typescript/v2/index.ts` | barrel：`export type * from "./X";` 由 `write_barrel` 自动生成 |
| `typescript/v2/codes.mts`、`methods.mts` | 有运行时真值（`export const`），故用 `.mts` 标明 ESM |
| `json/v2.json` | draft-07 schema，`definitions` 被**平铺到顶层**（好让 Python 生成平级模型而非嵌套） |
| `json/consts.json` | `{"codes":{…},"methods":{…}}` |

**驱动器 `scripts/write_schema.py`** 做了三件事，都在 Python 里（理由写在脚本头部）：

1. 用 `cargo test -p joyczl-protocol --lib export_tests::write_schema_fixtures
   -- --exact --ignored` 触发导出（生成逻辑只在 test 构建里存在，只能这么点着）。
2. 跑 `npx prettier` 排版 TS 产物（Rust 侧开不了 ts-rs 的 format feature）。
3. **比对**：`--check` 时把产物写到临时目录，与提交物逐文件比；有漂移就 exit 1。
   比对必须发生在**排版之后**，否则永远在拿「未排版的新产物」比「已排版的提交物」。

`--python` 额外用 `uv run datamodel-codegen` 从 `v2.json` 生成 SDK 的 `generated/v2.py`
（版本号写在生成物头部，钉死），并把 `consts.json` 渲染成 `Codes`/`Methods` 枚举。

CI 与 `just check` 都跑 `--check`，所以**协议改了没重新生成 = 构建红**。

## 1.6 为什么值得这么麻烦

三条代价换三份收益：

- 代价：改协议要跑一次生成、产物多、需要 Python 与 Node 参与构建。
- 收益 1：`memory/remember` 这个方法名全仓库只有一处定义，四端（Rust/TS/JSON/Python）
  不可能拼错。
- 收益 2：漂移在 diff 里现形——忘了同步 TS，CI 直接红，而不是等到前端运行时
  发现字段是 `undefined`。
- 收益 3：Python 侧不必手抄 12 个通知类（`protocol.py` 从 `RootModel` 摘联合类型），
  TS 侧不必手写 barrel。

**要加一个方法**：在 `v2.rs` 加方法常量 + `*Params`/`*Response` 类型 →
在 `export.rs` 的 `protocol_types!` 里加类型名 → 跑
`python3 scripts/write_schema.py --python` → 在 `app-server/dispatch.rs` 实现 →
在 SDK 的 `_RESPONSES` 表加配对。漏一步就有东西会红。
