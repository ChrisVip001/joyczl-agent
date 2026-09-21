# ④ provider 层：`joyczl-provider`

这一层是「Joy 的方言 ↔ 各家 API 的方言」之间的翻译层。整层最重要的一个设计决定是：
**loop 只说 Anthropic 的方言**，其它厂商的差异全部收敛在 `openai.rs` 的两个纯函数里。

目录：`joy-rs/joyczl-provider/src/`（`lib.rs`、`anthropic.rs`、`openai.rs`、`sse.rs`、
`embed.rs`、`mock.rs`、`error.rs`）。

## 4.1 中间方言：`ContentBlock` 与 `Message`

loop 与工具层只认这几个类型（`lib.rs`）：

```rust
enum Role { User, Assistant }

#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text { text: String },
    ToolUse { id, name, input: Value, extra: Option<Value> },
    ToolResult { tool_use_id: String, content: String },
}

struct Message { role: Role, content: Vec<ContentBlock> }
struct CreateRequest { model, system: Option<String>, messages, tools: Vec<ToolSchema>, max_tokens }
struct CreateResponse { stop_reason: StopReason, usage: Usage, content: Vec<ContentBlock> }
```

三个细节值得记住：

- `ContentBlock` 的 serde 标注让它的**序列化结果与 Anthropic 线上格式逐字相同**——
  所以 `anthropic.rs` 几乎不用做翻译，`openai.rs` 才要干活。
- `ToolUse.extra` 默认 `None` 且不序列化。它是给 Gemini 的 `thought_signature`
  留的槽：思考模型会在 tool_call 上带这个签名，**下一轮必须原样回传**否则 400。
  在 openai 方言里它进出 `extra_content` 字段；别的 wire 根本看不见它。
- `StopReason` 有 `#[serde(other)] Other` 兜底：服务端加一个新的停因不该把 Joy
  打崩，只是被归到「其它」。

## 4.2 `PROVIDERS` 表：12 家，一张常量表

`lib.rs:225`。每条 `ProviderInfo { id, wire, key_env, base_url, model, small_model }`：

| id | wire | key 变量 | 端点 |
|---|---|---|---|
| `anthropic` | Anthropic | `ANTHROPIC_API_KEY` | 官方默认 |
| `openai` | OpenAi | `OPENAI_API_KEY` | 官方默认 |
| `openrouter` | OpenAi | `OPENROUTER_API_KEY` | openrouter.ai |
| `gemini` | OpenAi | `GEMINI_API_KEY` | `…/v1beta/openai/`（用它的 OpenAI 兼容面） |
| `deepseek` | OpenAi | `DEEPSEEK_API_KEY` | api.deepseek.com |
| `minimax` | Anthropic | `MINIMAX_API_KEY` | `…/anthropic` |
| `kimi` | Anthropic | `MOONSHOT_API_KEY` | `…/anthropic` |
| `glm` | Anthropic | `ZHIPU_API_KEY` | `api.z.ai/api/anthropic` |
| `xai` | OpenAi | `XAI_API_KEY` | api.x.ai |
| `opencode_zen` / `opencode_go` | OpenAi | `OPENCODE_*_API_KEY` | opencode.ai |
| `ollama` | OpenAi | **空** | `http://127.0.0.1:11434/v1` |

`wire` 只有两个值，唯一用途是选客户端实现。国内几家（kimi/glm/minimax）之所以走
Anthropic wire，是因为它们都提供 Anthropic 兼容端点，而那个方言的 tool_use 表达
更直接。

**`key_env` 留空 = 声明「本地端点，不需要 key」**：`ProviderInfo::needs_key()`
就是 `!key_env.is_empty()`。这不只是个注释——`resolve` 与 openai 客户端都按它
分支（见 4.3、4.4）。

## 4.3 `resolve()`：一个链式 `Option` 决定一切

`lib.rs（resolve）` 的步骤：

1. `lookup(&settings.provider)`，失败 → `"未知的 JOY_PROVIDER '{x}'。可选：{全部 id}"`。
2. **key 解析**（优先级即链的顺序）：
   ```rust
   settings.api_key.clone()                    // JOY_API_KEY 最优先
     .or_else(|| env::var(info.key_env).ok().map(trim))
     .filter(|v| !v.is_empty())                // 空串不算 key
     .or_else(|| (!info.needs_key()).then(String::new))   // 本地端点：空 key 合法
     .ok_or_else(|| no_key_message(info))      // 其余：报「怎么配」
   ```
   注意最后一步：本地 provider 拿到的是**空字符串**而不是 `None`，客户端据此
   决定不发鉴权头；显式配了 `JOY_API_KEY` 的本地网关照用。
3. **base_url**：`JOY_BASE_URL` 优先，否则表里的默认。
4. **超时**：`reqwest::Client::builder().timeout(llm_timeout_secs)`，一个客户端
   一个超时，没有 per-request 覆盖。
5. 按 `wire` 造 `Arc<dyn Provider>`。
6. 返回 `Resolved { provider_id, client, model, small_model }`。

`Resolved` **手写 `Debug` 且故意不打印 client**——防止 key 进日志。

`no_key_message(info)` 是一条完整的操作指引：领 key 的链接（`key_url`）、
`export {key_env}=你的-key`（并说明 Joy 不读 `.env`）、其它可用 provider 列表、
`JOY_PROVIDER=<name>` 怎么切。错误信息按「用户下一步该做什么」来组织。

## 4.4 `Provider` trait：为什么手写装箱

```rust
trait Provider {
    fn create(&self, req) -> Pin<Box<dyn Future<Output = Result<CreateResponse>> + Send + '_>>;
    fn stream(&self, req, on_text: TextSink) -> Pin<Box<dyn Future<…> + Send + '_>> {
        Box::pin(self.create(req))   // 默认实现：不支持流式的就退回一次性
    }
}
type TextSink = Arc<dyn Fn(&str) + Send + Sync>;
```

用 `Pin<Box<…>>` 而不是 `async fn`（RPITIT），唯一原因是**要能装进
`Arc<dyn Provider>`**——`Resolved` 里存的就是 trait object。这个代价换来的是：
`Mock` 与真客户端是同一 trait 的两个实现，测试与评测可以直接换掉整层网络。

## 4.5 `openai.rs`：两个纯函数 + 一个 SSE 循环

### `to_openai(request)` — 方言翻译

- `system` 变成 **messages[0]** 的 `{"role":"system","content":…}`（openai 把 system
  当消息，Anthropic 当顶层参数）。
- assistant 的文本 + 全部 `ToolUse` **合成一条** assistant 消息：
  `content` 空则给 `null`，`tool_calls[]` 里每个是
  `{id, type:"function", function:{name, arguments: JSON 字符串}}`；
  `extra` 存在则塞进 `call["extra_content"]`。
- 一条 user 消息里若含 `ToolResult`，则**每个结果单独一条 `{"role":"tool",
  tool_call_id, content}`**，该消息里的纯文本被丢弃（openai 没有「一条消息既带
  文本又带工具结果」的形状）。
- 顶层：`max_completion_tokens`（不是 `max_tokens`，见下）、`tools` 非空才带。

### `from_openai(text)` — 反向翻译

- 含 `error` 键 → `ProviderError::Api`（接住 OpenRouter「200 + error body」这种）。
- 无 `choices` / 无 `message` → `Api`（带清楚的原因）。
- `stop_reason` 是**推出来的**：有 tool_calls 就是 `ToolUse`，否则 `EndTurn`
  （非流式响应里没有更细的信息）。

### `create_inner` 的那次重试

唯一的重试，条件很窄：**4xx 且 body 提到 `max_completion_tokens`** 时，把
`max_tokens` 换成和 `max_completion_tokens` 同名再发一次。为的是兼容仍在用旧参数
名的网关，仅一次、无退避。除此之外**全层没有重试**——429 不重试、超时不重试、
provider 错不重试，都原样冒到 loop，由用户看见。

### `stream_inner` 的 SSE 状态机

```
body["stream"] = true
body["stream_options"] = {"include_usage": true}      // 否则流式拿不到 token 数

状态：text: String、tools: BTreeMap<i64, PartialCall{id,name,arguments,extra}>、usage
循环（sse::data_lines）：
  "[DONE]"                    → break
  非 JSON                     → ProviderError::Parse
  顶层 usage                  → 记 input/output（最后一个 chunk 带用量、choices 为空）
  choices[0].delta.content    → 追加 text 并 on_text(piece)     ← 流式出口
  choices[0].delta.tool_calls → 按 index 取 entry：
                                  id / function.name 覆盖式赋值
                                  function.arguments 追加（分片！）
  finish_reason               → 记下来
结束后：
  text 非空 → push 一个 Text 块
  遍历 tools（BTreeMap 保证按 index 有序）→ arguments 解析失败回落 {}
    → ToolUse{ id, name, input, extra }
  stop_reason 映射：tool_calls/function_call→ToolUse、length→MaxTokens、stop→EndTurn，
                   空但 tools 非空 → ToolUse
```

`include_usage` 这个开关容易漏：不加它，流式对话的 usage 全是 0，trace 里的花费
就是假的。

### `authed()` — 空 key 不发头

```rust
fn authed(&self, b: RequestBuilder) -> RequestBuilder {
    if self.api_key.is_empty() { b } else { b.bearer_auth(&self.api_key) }
}
```

理由写在注释里：一个空的 `Authorization: Bearer ` 头比不带头更糟，有些网关会因此
回 401。本地端点（Ollama / LM Studio / vLLM）就走这条。

## 4.6 `anthropic.rs`：几乎没有翻译

- 请求：`{model, max_tokens, messages}`，`system` 存在才加顶层 `system`，
  `tools` **非空才加**（空数组有些端点会拒）。头是 `x-api-key` +
  `anthropic-version: 2023-06-01`（固定版本，不追新）。
- 应答解析几乎就是 `from_value::<Vec<ContentBlock>>` —— 因为中间方言本来就是
  照它设计的。`stop_reason` 字符串经 `stop_reason()` 映射，缺失默认 `EndTurn`，
  `usage` 解析失败就取默认（不因为少一个字段让整轮失败）。
- 流式事件：`message_start` 取 input token、`content_block_start` 建 tool 槽、
  `content_block_delta` 分 `text_delta`（调 `on_text`）与 `input_json_delta`
  （追加参数分片）、`message_delta` 取 stop_reason 与 output token、
  `error` 转 `ProviderError::Api`，其余忽略。

## 4.7 `sse.rs`：40 行的通用流解析器

`data_lines<S: Stream<Item = Result<Bytes, reqwest::Error>>>` 用
`futures_util::stream::unfold` 维护 `(stream, buffer, eof)` 三件状态：

1. buffer 里有换行 → 取出一整行；以 `data:` 开头的非空行才 yield（`event:`、
   注释行、空行直接跳过）。
2. 没整行但已 EOF → 若残留还以 `data:` 开头就交出去（处理「最后一个事件后面
   没有空行」的收尾），否则结束流。
3. 否则拉下一个 chunk 追加进 buffer；网络错 yield `ProviderError::Network`。

它不认识 `[DONE]`，原样交给调用方判断——解析器只负责「分帧」，语义留给上层。
两种 wire 共用它。

## 4.8 `embed.rs`：混合检索那条腿的来源

`Embedder::from_settings` 只需两个东西：`JOY_EMBED_MODEL` 与一个端点
（`JOY_BASE_URL` 或当前 provider 的默认端点）。key 的解析规则与 chat 侧一致
（`needs_key()` 才找环境变量）。

`embed(text)`：`POST {base}/embeddings {model, input}`，取 `data[0].embedding`
转 `Vec<f32>`；**向量为空 → `Api` 错误**（服务端给了 200 但没给向量，这是异常
而不是「空结果」）。

**刻意的克制**：没有重试、没有降级、没有缓存。向量是关键词检索的加分项，
算不出来时那条腿短了，检索本身照样工作（⑦ 里的 `search_hybrid` 兜住）。

## 4.9 `mock.rs`：让整层可测的替身

```rust
Mock::new(vec![...])        // 按序弹出应答；弹空 → Err("mock 的应答用完了")
Mock::streaming(vec![...])  // 流式模式
Mock::text("…")             // 便捷：EndTurn + usage 10/5
Mock::tool_use(id, name, input)  // 便捷：ToolUse + usage 20/8
pub received: Mutex<Vec<CreateRequest>>   // 收到过什么（断言提示词用）
```

两个为测试服务的细节：

- `take()` 时把请求 push 进 `received`，于是测试能断言「我们到底给模型看了什么」
  （eval 的场景断言 `prompt_contains` 就建在它上面）。
- 流式模式下把文本**两字一切**、每块之间 `sleep(5ms)`：模拟真实 SSE 的 await 点，
  给「打断」测试一个确定性的插入时机（否则取消令牌没有机会在流中间生效）。

**这一层的不变量**：loop 只见中间方言；厂商差异只在两个纯函数里；没有静默重试；
流式与非流式产出同一个 `CreateResponse`；空 key 不发空头。
