# ③ 配置层：`joyczl-config`

一个 400 行的单文件 crate，把「Joy 能被配置成什么样」这件事收敛到一处。

目录：`joy-rs/joyczl-config/src/lib.rs`（外加协议层里的 `SettingsView` /
`SettingsPatch` 类型）。

## 3.1 三条设计决定

1. **环境变量是唯一来源**，`from_env()` 是唯一读取路径（不另有配置文件、不另有
   优先级规则）。模块注释那句「读得懂这个文件，就读得懂 Joy 能被配置成什么样」
   就是这个意思。
2. **启动时读一次，之后不可变**。`Settings` 是普通结构体，没有全局可变状态。
   好处是旋钮集合在编译期穷尽、运行中不会配置漂移。
3. **Joy 不读 `.env` 文件**。仓库根的 `.env.example` 是给人 `source` 的模板，
   不是自动加载的配置（`docs/configuration.md` 与那份模板头部都写明了）。

## 3.2 三个读取助手（语义都藏在小细节里）

```rust
env(name)     -> var(name).ok() → trim() → filter(!is_empty)   // 空串 = 没设置
env_bool(name)-> matches!(env(name).as_deref(), Some("1")|Some("true")|Some("yes"))
env_int(name, default) -> env(name).and_then(|v| v.parse().ok()).unwrap_or(default)
```

- **空串视为未设置**：`.env` 里写 `JOY_MODEL=` 这种留空行不会把模型名变成空字符串，
  而是让默认值生效。
- 布尔只认三种拼写（`1`/`true`/`yes`），`0`/空/其它一律 false；不猜 `on`/`enabled`。
- 整数解析失败回默认，**不 panic** —— 一个手滑的数值不该让 Joy 起不来。

## 3.3 `Settings` 的全部旋钮

`from_env()` 从 `Settings::default()` 出发逐字段覆盖。当前的 21 个：

| 字段 | 环境变量 | 默认 |
|---|---|---|
| `provider` | `JOY_PROVIDER` | `anthropic` |
| `api_key` / `base_url` / `model` / `small_model` | `JOY_API_KEY` / `JOY_BASE_URL` / `JOY_MODEL` / `JOY_SMALL_MODEL` | 无（用 provider 默认） |
| `home` | `JOY_HOME` | `./.joy` |
| `llm_timeout_secs` | `JOY_LLM_TIMEOUT` | `120` |
| `max_iterations` | `JOY_MAX_ITERATIONS` | `10` |
| `max_tokens` | `JOY_MAX_TOKENS` | `8192` |
| `history_turns` | `JOY_HISTORY_TURNS` | `12` |
| `consolidate_every` | `JOY_CONSOLIDATE_EVERY` | `6` |
| `retrieval_top_k` | `JOY_RETRIEVAL_TOP_K` | `4` |
| `apple_calendar` / `google_calendar` / `experimental` / `graph_workflows` | 同名 `JOY_*` | `false` |
| `embeddings_enabled` / `embed_model` | `JOY_EMBEDDINGS` / `JOY_EMBED_MODEL` | `false` / 无 |
| `exec_enabled` / `exec_allow` / `exec_timeout_secs` | `JOY_EXEC` / `JOY_EXEC_ALLOW` / `JOY_EXEC_TIMEOUT` | `false` / 空表 / `30` |

两个值得注意的：

- `exec_allow` 是**逗号分隔切出来的 Vec**。空表 = 什么都不放行（默认拒绝）——
  这个语义在 ⑥ 里是安全边界的一部分。
- `llm_timeout_secs` 与 `exec_timeout_secs` 是 i64，但用 `env_int`（i32）读再转
  i64——够用，且省一个 `env_i64`。

`ensure_home()` 建 `home/traces`、`home/outbox`、`home/skills` 三个子目录；
`view()` 产出 `SettingsView`（`config/read` 的载荷），把两个 `Option<String>`
补成空串、把 `home` 转成字符串。

## 3.4 `SettingsPatch`：让 `config/write` 活过重启

环境变量是地基，但驾驶舱的「设置」页需要一个**能改、能存、重启还在**的层。
于是有了补丁：`<home>/settings.json`，一份字段全为 `Option` 的 `SettingsPatch`
（类型定义在协议层，因为它要过 JSON-RPC）。

三个函数的语义（`config/lib.rs`）：

```rust
patch_path(home)        -> home.join("settings.json")
load_patch(home)        -> 读不到 / 解析不了 都当「没有补丁」（unwrap_or_default）
save_patch(home, patch) -> 空补丁 = 删掉文件（「这个目录里没有覆盖」这件事成立）
apply_patch(patch, s)   -> 只有 Some 的字段才发言
```

`apply_patch` 里有一个刻意的细节：`model` / `small_model` 走 `non_empty()`，
**空串（含全空白）表示「清掉显式覆盖，回到 provider 默认」**。
于是「把模型名清空再保存」是有意义的一步，而不是把模型设成 `""`。

`SettingsPatch` 自己带两个方法（定义在协议层 `v2.rs`，因为它是协议类型）：

- `is_empty()`：所有字段都是 `None`（`save_patch` 用它决定删文件）。
- `merge_newer(&mut self, newer)`：用 `newer` 里**非空**的字段盖掉自己。
  语义是「已保存的补丁是新补丁的地基」，不是反过来。

## 3.5 三级叠加：环境 → 已存补丁 → 新补丁

**启动时**（`app-server/lib.rs（open）`）：

```
settings = Settings::from_env()      // 地基
saved    = load_patch(home)          // 上次保存的覆盖
apply_patch(&saved, &mut settings)   // 叠上去
ensure_home(); state::open(...)
```

**`config/write` 时**（`app-server/lib.rs（apply_config_patch）`）顺序是有讲究的：

1. `validate_patch` **先校验**：provider 必须在 `PROVIDERS` 表里；数值有下界
   （`maxIterations≥1`、`maxTokens≥128`、`historyTurns≥0`、`consolidateEvery≥1`、
   `retrievalTopK≥1`）。失败就返回错误，**什么都没改**——补丁是全有或全无的。
2. `saved = load_patch()` → `saved.merge_newer(patch)`（累计，不是替换）。
3. `save_patch(&saved)` **先落盘**。落盘失败就不动内存：「改了但不记得」比
   「没改成」更撒谎。
4. `apply_patch(&saved, &mut settings)` 套到内存。
5. 重新 `resolve(&settings)`（换了 provider/model 就得换客户端；解析失败不致命，
   只 eprintln 提示，`turn/*` 会报「缺 key」）。
6. `set_settings` + `set_resolved`（两把 `RwLock`，见 ⑨）。

**因此有一条容易踩的规矩**：对某个字段而言，**保存过的补丁比 `JOY_*` 说话更响**，
直到把它清掉（空串清 model 覆盖；全空补丁删文件）。`docs/limitations.md` 里记着
这一条与「`config/write` 不重建工具表」这个边界：改了 `JOY_EXEC*` 要重启进程。

## 3.6 测试

`config/lib.rs` 底部 6 条：`apply_patch` 的叠加与空串清除、`merge_newer` 的新旧
优先级、补丁落盘往返、空补丁删文件、坏文件读成「没有补丁」、`view` 字段完整。

**这一层的不变量**：所有旋钮都能在 `.env.example` 与 `docs/configuration.md` 里
找到；解析永不 panic；补丁永远「先校验、再落盘、后生效」。
