# M4 选题清单 —— 来自 openhuman 的实现级侦察

> 状态：规划笔记（非产品文档，暂不英译）。证据全部来自 openhuman 仓库
> 实测路径+行号；vendor/ submodule 未 checkout 的部分已注明「据注释/文档」。

## 先纠正一个直觉：fleet 不是「单进程几百个 agent」

openhuman 的 `fleet.rs` 头注释自述：**process-per-user supervisor + 反向代理**
——每个租户一个 `openhuman-core run --headless-api` 子进程 + 独立端口 +
独立 workspace 卷。这正是 joy hub 的现状，而且 fleet 自己标注
"This MVP does not yet ... not a production multi-tenant security boundary"、
崩溃后只摘路由不重启不补账（`fleet.rs:708-721`）——**比 joy 的模型更弱**
（joy 有守护锁、启动对账、调度补跑）。

→ **结论：joy 的「每用户一个 home + 子进程」被同类最优实现反向验证了，不是要被替换的旧设计。**

真正实现高密度的是另一条线：`ProfileRuntime`/`ProfileHost`（SaaS 模式）。

## 抄什么（按性价比排序）

### 1. hub 资源池的「slot 表 + LRU 驱逐 + in-use pin」★ 最小第一步

- 证据：`crates/openhuman-core/src/core/runtime/saas.rs:243`（`max_profiles_open` 默认 256）、
  `host.rs:631-637`（满了关最久未用的 idle）、`host.rs:4-6`（in-use 永不驱逐）、
  `embed/tests/profile_turn_serialization.rs`（per-profile 串行、跨 profile 并行）。
- joy 现状：hub 已有懒加载 + 每用户独立 home/子进程，缺的只是
  **池上限 + idle 驱逐 + 有 turn 在跑时 pin 住** 三条策略。
- 最小抄法：hub 的 client 表加 `JOY_HUB_MAX_OPEN`（默认 32？）与
  `JOY_HUB_IDLE_SECS` 两个旋钮；满时 LRU 关 idle 实例；in-use pin。
  纯 HashMap + 时间戳，openhuman 整套约 200 行。
- **不做**：lease/心跳/多节点接手（`lease.rs`）——那是跨 hub 节点分片才
  需要的复杂度税；进程级 SaaS 模式锁定（`profiles.rs:23-33` 的 one-way
  switch）——joy 把池做成 hub 内普通结构，不引入全局模式。

### 2. 调度状态进 SQLite + 「dispatch 先推进 next_run」+ delivery 模式

- 证据：`cron/README.md:437-442`（recurring job 的 `next_run` 在 dispatch 时
  推进，at-most-once per slot）；`:163-188`（`delivery: none|proactive|origin`
  与 NO_REPLY 抑制）。
- joy 现状：`schedules-state.json` + 重启补跑（至少一次）。
- 最小抄法（M4 候选，二选一或并存）：
  - a) 调度状态从 json 挪进 state.db 一张表，取作业时先 UPDATE 推进
    `next_run` 再执行——幂等防重放，让「补跑 or 放弃」变成每作业一个字段；
  - b) 每作业加 `delivery` 与 NO_REPLY——空输出不投递只记 run 历史，
    一句话少很多打扰。
- 注意取舍：joy 的补跑（至少一次）对「晨报」类任务是对的用户承诺，
  openhuman 的放弃（至多一次）对通知类任务更合理——**按作业类型给字段，
  不要全局二选一**。

### 3. skills-as-runs：技能的终点是 Markdown → 独立 agent run

- 关键发现：**QuickJS 已被原作者删除**（`skills/webhooks/README.md:96`：
  "the QuickJS skill runtime was removed"）——joy「技能是 Markdown、无代码
  执行」的选择被对方的演化终局验证。
- 证据：`skills/README.md:14-15`（*"starts a separate agent run. Skill bodies
  are never spliced into a chat turn."*）、`runtime/README.md:14-19`
  （orchestrator agent + body 作为 task prompt + 迭代上限 + cancellation token race）。
- 最小抄法（joy 已有 $skill-name 装载；补的是「把技能当工作流跑」）：
  触发时起独立 turn（子代理 runner 现成），body 作为 prompt、不拼进当前
  对话；run 日志落 `<home>/skills/.runs/`；迭代上限复用 JOY_DELEGATE 那套。

### 4. 杂项（顺手级）

- **带租户凭证的探活**（`fleet.rs:356-359`）：hub 健康检查用「带该用户
  token 的真请求」而非裸 200——防「旧进程占着端口但拒绝新凭证」。
  joy hub 加一条健康检查时抄。
- **marker 文档式一次性导入**（`storage.md:146`：absent-only 写入 +
  `<domain>@<digest>` 标记，boot 见 marker 即跳过）：将来迁移
  schedules-state.json 或别的旧格式时直接套用。

## 不抄什么（及为什么）

| 不抄 | 理由 |
|---|---|
| 存储 URL 多驱动（6 个 store trait + Cargo feature 门控 + conformance 套件） | 为「桌面单机 + SaaS + 云」三形态服务；joy 状态固定 sqlx SQLite，抄了等于为不存在的需求维护抽象 |
| Scope/tenant 存储隔离 + fenced_backend | 单用户 `Scope::local()` 恒定；唯一可取的是「若将来单进程多用户，所有 per-user 状态必须经 tenant key 寻址、绝不 ambient fallback」（`profiles/README.md:267-271`）——记为纪律，不是代码 |
| bus 契约模块化（tinyflows/tinychannels 走 v6 bus + 两小时 deadline） | 为 embedder 热插拔付的税；joy 调度器做成模块内函数即可 |
| 进程内抢占调度/优先级 | openhuman 也没有——调度单位是 profile 不是 agent，跨 profile 天然并行就够了 |
| QuickJS/语言级沙箱 | 原作者已删；脚本安全交给 agent 层（SecurityPolicy + shell 超时），joy 的沙箱 exec 已覆盖 |

## 对 M4 的修订建议

原 M4（会话亲和多 worker / 容器隔离 / 配额强制）按此侦察修订为：

1. **M4a hub 资源池**：slot 上限 + LRU/idle 驱逐 + in-use pin（抄 §1）——
   资源密度问题的最小正确解，~200 行；
2. **M4b 调度升级**：状态进 SQLite + per-作业 delivery/补跑字段（抄 §2）；
3. **M4c skills-as-runs**：技能作为独立 run 执行 + run 日志（抄 §3）；
4. 容器隔离与配额强制：**继续推迟**——openhuman（最激进的同类）也没有
   配额强制；容器只在 profile 级 Docker 沙箱里出现（`host_shell`），对 joy
   现有 seatbelt/bwrap 沙箱没有增量。
