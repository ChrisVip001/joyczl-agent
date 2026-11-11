# 用户中心与会话（设计稿 v0.2）—— 本轮只做实「一个会话」

> 输入：① 与 ChatGPT 确定的《Joy Product Philosophy & Architecture Principles》（方向基线
> v1.0，以下简称「基线」）；② 第二份对话——**对公开源码的静态架构审查**（能力矩阵、五个
> 优先问题，结论见 §1.3）；③ 本仓库 0.14.0 的真实代码。
>
> 本文是**决策文档**，不是已实现功能。所有「现状」都附代码位置，「设计」都写成可验收的
> 断言。文档语言只用中文，落成 `docs/` 正式书册时再补英文正本与 `.zh.md` 孪生。

---

## 范围（2026-11 决定：空间先挂起）

| 项 | 状态 | 说明 |
|---|---|---|
| 会话元数据：名字、工作目录、在跑的轮次 | **已完成**（0.15） | 单人单机也缺这些（见 §1.1）；归属列留到共享阶段 |
| 会话级 turn 门 + 运行中补充（Steering） | **已完成**（0.16） | 审查的 P0，与是否共享无关；`follow-up` 挪后 |
| 团队空间、成员与角色、接管、迁入 | **挂起** | 设计保留（§2–§3），代码一行不动：不建 `spaces` 账目、不加 `X-Joy-Space`、不改 hub 路由 |
| 只读链接、跨人审计、工具执行账本 | 挂起 | 只有共享才需要 |

一句话：**先把「一个会话」做实，共享留给下一阶段。** 本文 §2、§3.3–§3.9、§5.3–§5.6 都按
「未来演进」读，不是本轮要写的东西；写它们是为了将来动手时不必重新推导一遍。

---

## 0. 结论先行

1. 本轮只做两件，都不涉及多用户：**会话元数据**（0.15）与**会话门 + Steering**（0.16）。
   不动 Loop/Tools/Graph/Memory，只给 `joyczl-state` 加一张元数据表、给 app-server 加一个
   会话级互斥与几个只给人用的方法。
2. 为什么是这两件：会话现在是 `chat_log` 上的一个标签——没有名字、没有工作目录、查不到
   「这个会话在跑什么」；同一会话的并发 turn 也没有完全串行化，运行中补充一句只能中断重来。
   这两条与共享无关，是**个人助理本身的缺口**（§1.1、§1.3）。
3. 挂起的部分不会白设计：0.15 的 `sessions` 表就是将来 `owner / visibility / holder` 的落点，
   0.16 的 turn 门就是将来 `claim` 的实现底座。到时候只加列、加路由，不推翻。
4. 顺序仍是：**先会话，后门，最后才谈共享。** 一旦要开放共享，`reconcile` 的无条件续跑必须
   先改（§8），否则 A 的进程重启会替 B 继续干活——那是事故，不是功能。

---

## 1. 现状能力矩阵（对照基线）

| 基线要求 | 现状 | 证据 | 差距 |
|---|---|---|---|
| 统一 Assistant Runtime，各入口不各自持有状态 | 已成立：CLI / 驾驶舱 / 网关全是 app-server 客户端 | `docs/architecture.zh.md:16-18` | 无 |
| 跨入口会话连续性 | 会话是 `chat_log.session_id` 标签，消息在同一 `state.db` | `joyczl-state/src/chat.rs:1-4`、`:302-359` | 会话元数据缺失（见下） |
| 聊天记录 ≠ 执行状态 | 已分离：`turn_checkpoints` + `agent_tasks` | `migrations/0013_turn_checkpoints.sql`、`0017_task_engine.sql` | 无 |
| 执行状态可恢复 | 有：启动对账 + `task/resume` | `app-server/src/resume.rs:23-40` | **无条件续跑**，多驱动下会替别人干活 |
| 记忆分层（长期 / 会话 / 任务 / 经验） | 长期记忆与经验有；**会话维度只有摘要** | `facts`/`episodes` 无 `session_id`（`0001_init.sql:5-42`）；`context_rollups.session_id` | 记忆是 per-home 全局，这是共享空间的正确语义，但必须写清 |
| 权限与用户控制 | hub 已有账号 + per-device token + 每用户一个 home | `ops/src/hub.rs:105-145`、`ops/src/lib.rs:205-251` | 账号无「组」，会话无归属 |
| 后台持续、人工介入继续 | 任务实体 + `needs_attention` | `0017`、`verify.rs`、`observe.rs` | 接手时没有「谁在驱动」的概念 |

### 1.1 会话这一层的硬缺口

| 缺口 | 事实 | 证据 |
|---|---|---|
| 没有 `sessions` 表 | 会话只是标签。标题 = 第一条 user 消息；空会话 = `"(空会话)"` | `chat.rs:324-359` |
| 「新建会话」不落库 | `session/new` 只发一个标签，第一条消息才算存在 | `dispatch.rs:159-168` |
| 会话 id 无中央分配 | 服务端 `s{millis}`；前端另生成 `s-<base36>-<uuid4>` | `dispatch.rs:493-499`、`main.ts:905` |
| 工作目录不持久 | cwd 是 per-turn 参数，用完即弃；localStorage 只有一个全局 `joy.workspace` | `v2.rs:884-888`、`main.ts:94-112` |
| 无归属、无审计 | `chat_log` 无 `actor`；`approval_log` 无 `session_id`/`actor` | `0011_approval_log.sql:10-19` |
| 在跑的 turn 无法按会话查 | 内存表键是 `turn_id`，不含 `session_id` | `app-server/src/lib.rs:166` |

### 1.2 可写范围（共享时最容易误解的一条）

`writable_roots = [ctx.cwd] + JOY_EXEC_WRITABLE_ROOTS`；`ctx.cwd` 为空时**任何写入都被拒绝**，
home 与进程 cwd 刻意不算根（`joyczl-tools/src/fs_write.rs:72-79`）。
`JOY_WRITE` 只是「是否注册写工具」的开关，不是范围。
→ 推论：会话必须持久化一个 `cwd`，否则第二个人接手时「能做什么」取决于他随手选了哪个目录。

### 1.3 第二份材料：源码级审查的结论（已读全文）

第二份对话是对公开源码逐文件的静态审查（不是产品文档），核心判断与我们一致——不推倒
Runtime、不新增重复 crate，按现有位置增强——但把「下一步做什么」排成了五个优先级：

| 审查结论 | 依据（本次实读代码核对） | 对本文的意义 |
|---|---|---|
| Loop 已非简单 ReAct：流式 + 中断 + 恢复，**不要改成 Plan-and-Execute** | `joyczl-loop`、`turn.rs` | 本文不动 Loop |
| App Server 是架构资产：唯一 State、统一事件通道，入口不得各自持有状态 | `docs/architecture.zh.md:16-18` | 与 §2「共享空间」同源 |
| **Task 的创建权属于人，不属于 Joy**（`task.rs` 明写 human-only） | `task.rs:11`、`v2.rs:34` | 与本文「能力由人打开」一致；即使将来让 Joy 参与建任务，也必须过 Runtime 授权 |
| Checkpoint 只保证**工作记忆恢复**，不保证**外部副作用 exactly-once**：副作用已发生但 checkpoint 未落盘时崩溃，恢复可能重放该工具 | `turn.rs:1329-1343`（写失败只打一行 `checkpoint 没写进去（照常跑…）` 后 `return`） | **这是接管的硬约束**，见 §5.4 安全边界 |
| Interrupt ≠ Steering：能取消，不能「补充一句让它改方向」；同一会话的并发 turn 也未完全串行化 | `docs/limitations.zh.md:258-265`、`turn.rs` 无 steer 路径 | 接管与 Steering 共用「会话级 turn 门」，见 §4.1 |
| Memory 缺项目记忆与执行经验关联；附件不随会话恢复重放、Episodes 不向量化 | `limitations.zh.md:258-261, 298-303` | 空间 home 就是项目记忆的天然边界，见 §4.1 |
| 后台执行成熟但通知 best-effort：无重试、无投递账本 | `limitations.zh.md:31-44` | 交接不能只靠外部通知，见 §5.4 |
| 优先级：P0 助理自主性 / P0 Steering / 高 副作用恢复 / P1 个人与项目记忆 / P1 主动服务 | — | 与本文的插入点见 §4.1 |

**三处需要修正或读法要变的地方**（审查基于较早的公开版本，0.14.0 已前进）：

1. 「一个 Session 最多一个活动 Task」**已不成立**：`0017` 删掉了 `UNIQUE(session_id)` 的部分
   索引，`agent_tasks.session_id` 允许为空。这对本文是**好消息**——一个工作会话可以同时挂
   多个任务，不同人可以各自负责其中一件，不必挤在一条执行线上。
2. 「Task 创建权在人」**仍然成立**（`task.rs:11`）。所以共享空间里「让 Joy 自己接活」不是
   本轮的补充项，别顺手放开——那会让团队空间里出现无人授权的任务。
3. 「checkpoint 写失败只记日志」**仍然成立**（`turn.rs:1341`）。在单机单人场景这是降级；
   在共享会话里，它意味着**别人接手时会重放一次结果未知的工具调用**。所以接管不能随便
   在任意时刻发生。

---

## 2. 关键判断：共享的粒度是「空间」，不是「会话行」

> 本节与 §2.1 属于**挂起部分**，保留下来是为了将来动手时不必重新推演一遍。

一条会话「能接着干活」需要五样东西同时在场：

```
会话需要的东西                    现在住在哪
─────────────────────────────────────────────────────────────
1 对话历史 + 滚动摘要             <home>/state.db: chat_log, context_rollups
2 执行检查点（做到哪一步）        <home>/state.db: turn_checkpoints
3 任务与交付物                    <home>/state.db: agent_tasks / task_artifacts
4 可写目录（范围即权限）          TurnStartParams.cwd → ToolCtx.cwd（不落盘）
5 事实记忆（为什么这么做）        <home>/state.db: facts / episodes
```

五样全在 home 里，而 home 与 app-server 是**一对一**且带独占锁（`<home>/app-server.lock`，
`app-server/src/lock.rs:17-84`）。所以「两个人一起干同一件事」不能靠复制 `chat_log`：副本没有
检查点、没有交付物、没有记忆，写完的文件也还在原来的机器上——那是转发聊天截图。

一次性把会话搬到另一个空间是另一回事，可以做，但它只是「交接上下文」，不是「共同工作」；
差别与做法见 §2.1。

所以可行的只有三条路，只有第三条成立：

| 方案 | 做法 | 判断 |
|---|---|---|
| A 单库多租户 | 全站一个 `state.db`，表加 `owner`/`tenant` 列 | **否决**：推翻 local-first 与「一个 home 一个进程」的全部既有不变量，等于重写状态层 |
| B 每个团队独立 hub/端口 | 团队空间是另一个 Joy | **否决**：基线的「Single Identity / 统一入口」直接不成立 |
| C 共享空间 = 共享 home | 团队拥有自己的 home，成员按角色进入；会话在空间内 | **采纳**：复用 per-tenant home 这一现成答案（`hub.rs:1-17`），隔离是天然的 |

于是「个人 / 工作」不是会话的两种类型，而是**两种空间**：

```
<root>/
  users.json                 身份 + 空间账目（v3）
  <user>/                    个人空间：个人会话、个人记忆、个人模型与密钥
  t/<team>/                  工作空间：工作会话、团队记忆、团队模型与工作目录
    state.db
    app-server.lock
```

这也是最符合基线 4.6（用户数据由用户控制）的形态：工作空间的记忆与文件属于团队，个人记忆
永远不出个人空间。

### 2.1 「个人会话转成工作会话」只能是**迁入**，不能是挂载

需求里有一句很关键：个人会话可以改变属性、放进团队共享空间。实现方式只有两种：

| 做法 | 机制 | 判断 |
|---|---|---|
| 挂载（同一条会话被两个空间引用） | 团队空间持有一个指向个人 home 的引用，消息仍写回个人 `chat_log` | **否决**：两个 home 各有一个 app-server（还各自持锁），同一条会话就有了两个写入口、两套 memory 视图、两个 cwd —— 这正是 §2 否决过的「共享会话行」 |
| 迁入（复制一次 + 原会话封存） | 在目标空间**新建**一条工作会话，把内容搬过去；原会话变只读并指回新会话 | **采纳**：只有一份权威数据，任何时刻只有一个 home 拥有它 |

所以「改属性」在实现上是一次**显式的、由人发起的、只在会话空闲时执行**的迁移动作：

```
个人 home                          团队 home
session A (archived)               session B (新)
  └─ moved_to = "team/x/B"  ←──┐     └─ origin = "personal:alice/A"
                               └────────  bundle（消息 / 任务 / 摘要 / 文件清单）
```

原会话不删除、不隐藏，只在列表上标成「已迁至 <团队>/<标题>」并变只读。这样 A 的对话历史在
个人空间仍然可查（否则「我把东西交出去了，自己反而看不到」），但它不再接受新消息——两个写
入口的问题从根上消失。

反方向（团队 → 个人）**不做**：那会把团队记忆与文件带出空间边界。要带走就是导出 + 由人自己
新建，不留自动通道。

代价要认：迁入**带不走 checkpoint**（不重放未知副作用），也带不走没勾选复制的文件。所以它交接
的是**上下文**，不是**执行进度**——这正是迁入后的任务一律降级为 `needs_attention`、由人重新
指定 cwd 再继续的原因。

---

## 3. 用户中心设计

> 本轮执行的只有：§3.2 的第一版列（`id/title/cwd/status/时间`）、§3.5 里的 `session/*` 与
> `turn/list`。**空间、角色、接管（§3.3–§3.4）、迁入（§3.9）全部挂起。**

### 3.1 三个概念

| 概念 | 定义 | 归属 |
|---|---|---|
| **身份 user** | 账号，PBKDF2 口令 + 若干设备 token（已有） | `users.json` |
| **空间 space** | `personal:<user>` 或 `team:<team>`；一个空间 = 一个 home = 一个 app-server | `users.json` 的 `spaces` 账目 |
| **会话 session** | 空间内的一行元数据 + `chat_log` 上的标签 | 空间内 `state.db` |

驾驶舱顶部的空间切换器决定「新建会话建在哪」，会话列表按空间分组。`X-Joy-Space` 请求头
（缺省 `personal`）把请求路由到对应 app-server。

### 3.2 数据模型（迁移 0018）

```sql
-- 会话终于有了一行自己的记录。旧会话由 chat_log 反填，见 §6。
CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,
    owner       TEXT,                 -- 创建者；个人空间 = 用户名，工作空间 = 成员名
    title       TEXT,                 -- 人起的名字；NULL 时仍回退「首条用户消息」
    cwd         TEXT,                 -- 本会话的工作目录（唯一可写范围的根），可空
    visibility  TEXT NOT NULL DEFAULT 'private'
                CHECK (visibility IN ('private', 'space', 'link')),
    status      TEXT NOT NULL DEFAULT 'idle'
                CHECK (status IN ('idle', 'running', 'waiting', 'archived')),
    holder      TEXT,                 -- 当前驱动者；NULL = 无人驱动
    lease_until TEXT,                 -- 租约到期（strftime('%Y-%m-%dT%H:%M:%fZ','now')）
    origin      TEXT,                 -- 迁入来源："personal:alice/s123"；null = 原生
    moved_to    TEXT,                 -- 迁出目标："team:joy/s456"；非空即只读
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    last_at     TEXT
);
CREATE INDEX IF NOT EXISTS sessions_recent ON sessions(last_at DESC);

CREATE TABLE IF NOT EXISTS session_participants (
    session_id TEXT NOT NULL,
    actor      TEXT NOT NULL,
    role       TEXT NOT NULL DEFAULT 'writer'
               CHECK (role IN ('owner', 'writer', 'reader')),
    joined_at  TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (session_id, actor)
);

ALTER TABLE chat_log ADD COLUMN actor TEXT;          -- 谁说的这句话
ALTER TABLE approval_log ADD COLUMN session_id TEXT; -- 哪个会话的审批
ALTER TABLE approval_log ADD COLUMN actor TEXT;      -- 谁批的（共享空间审计的最低要求）
```

> 这是**全貌**，不是本轮要执行的迁移。本轮只落 `id / title / cwd / status / created_at /
> last_at`；其余列、`session_participants` 与 `chat_log.actor` 在共享阶段用 `ALTER TABLE` 加。
> 写全是为了将来不用重新推导。

不变的部分：`chat_log.session_id` 仍是纯 id，**不拼接空间前缀**。空间只在 ops 的路由层与
驾驶舱里存在。协议不因为多用户而变成两套。

### 3.3 接管协议（单驱动 + 租约）

```
        session/claim (CAS)
 idle ──────────────────▶ running(holder=A, lease=now+90s)
   ▲                            │  ├─ 每轮 turn 心跳续租
   │                            │  └─ 租约过期 / session/release / session/handover
   └────── idle(holder=NULL) ◀──┘
```

规则（每一条都要有测试）：

1. `turn/start` 前先 `claim`：`UPDATE sessions SET holder=?, lease_until=? WHERE id=? AND
   (holder IS NULL OR holder=? OR lease_until < now)`。0 行 = `SESSION_BUSY`，错误里带 holder
   与剩余秒数，驾驶舱据此显示「阿明正在使用，12 秒后可接手」。
2. 租约 90 秒，turn 每完成一轮工具边界续一次（与 checkpoint 同频，不额外写库）。
3. `session/release` 立刻放权；`session/handover {to}` 把 holder 直指某人（「我下班了」）。
4. 进程崩溃不需要清理：租约自然过期即可接手，这与现有 `reconcile` 的哲学一致。
5. `reader` 不能 claim；`writer` 可以；`owner` 还能改 `visibility`、工作目录、参与者。
6. `session/update`、`handover`、参与者管理**只暴露给驾驶舱与 CLI**，不注册成模型工具
   （沿用「能力由人打开」的不变量）。

### 3.4 权限模型

| 层 | 角色 | 能做什么 |
|---|---|---|
| 空间 team | `owner` / `member` | owner 管成员、空间级配置（模型、`JOY_WRITE`）；member 进出空间 |
| 会话 session | `owner` / `writer` / `reader` | owner 改元数据；writer 驱动与写文件；reader 只读 |
| 可见性 | `private` / `space` / `link` | private = 仅参与者；space = 空间成员可见可 claim；link = 只读短期 token |

**必须写清的边界**：模型与密钥是 **home 级**的。团队空间一套 provider 由空间 owner 维护，
成员带自己的 key 进来不生效；工具开关（`JOY_EXEC`/`JOY_WRITE`/`JOY_HOOKS`）同理是空间级。
用户个人空间的一切保持不变。

### 3.5 协议增量（`joyczl-protocol/src/protocol/v2.rs`）

| 方法 | 变更 | 备注 |
|---|---|---|
| `session/new` | + `title?` `cwd?` `visibility?` | **真的落库**；id 由服务端分配（`s{millis}{4位随机}`），前端不再自造 |
| `session/list` | + `visibility?` `mineOnly?` | 返回 join 后的元数据 |
| `session/get` | 新增 | `{id,title,owner,cwd,visibility,status,holder,leaseUntil,participants}` |
| `session/update` | 新增 | 只给人：标题 / 可见性 / cwd |
| `session/claim` | 新增 | → `{ok, holder, leaseUntil}` 或 `SESSION_BUSY` |
| `session/release` | 新增 | — |
| `session/handover` | 新增 | `{to}`，只给人 |
| `turn/list` | 新增 | 现在 `turns` 只按 turn_id 存（`app-server/src/lib.rs:166`），加 `session_id` 后才能回答「这个会话在跑什么」 |
| `session/export` | 新增 | 只给人：产出自包含 `SessionBundle`（§3.9），`idle` 才允许 |
| `session/import` | 新增 | 只给人：投递 bundle，`origin` 做幂等键，单事务 |
| `SessionSummary` | + `owner?` `cwd?` `visibility?` `status?` `holder?` `participants?` | 兼容：全部 `Option` + `#[serde(default)]` |

改完必跑 `just write-app-server-schema` 并提交生成物（`AGENTS.md:52-55`）。

### 3.6 ops / hub 改造

1. `users.json` 升 **v3**：`{"users":{...}, "spaces":{team:{name, owner, members:{u:role}, created_at, policy:{write, exec}}}}`。
   v2 在加载时内存迁移（与 v1→v2 同样的做法，`hub.rs:161-179`）。
2. `Hub::client(&user)` → `Hub::client_for(&user, &space)`：
   * `personal` → 池键用 `user`（现状不变）；
   * `team` → 池键用 `space`，**per-space 单例**。原因：一个 home 只允许一个 app-server
     （`lock.rs`），按 user 各起一个会直接撞锁。
3. 团队实例**不可按闲置驱逐**（`JOY_HUB_IDLE_SECS`）：驱逐会打断别人正在跑的 turn。
   个人空间维持现有 LRU；团队实例常驻，只在无人且无活跃 checkpoint 时回收。
4. `authorize` 增加空间解析：从 `X-Joy-Space` 取 space → 校验成员 → 注入 `(app, space, user)`
   （`ops/src/lib.rs:205-251`）。
5. 审计入口：`/api/approvals`、`/api/usage` 在团队空间按空间汇总，不再有「谁批的」盲区
   （依赖 §3.2 的 `approval_log.actor`）。
6. 第一版团队空间**不开放** `/api/hook/*` 与 `/api/run`（与现状 hub 一致 404），
   避免新增外网入口。

### 3.7 驾驶舱

- 侧栏两段：`个人` / `<团队名>`；`新建会话` 属于当前空间，标题旁一个空间徽标。
- 会话头：标题（可改名）、可见性徽标、参与者首字母、驱动状态
  「阿明正在使用 · 12 秒后可接手」；`接手` 按钮在可接手时可点，占用中禁用并说明原因。
- 工作目录从 localStorage 的**全局一个**改为**会话一个**（存 `sessions.cwd`）；
  切换会话即切换目录，个人空间首次使用回退到最近目录（`workspaces.json` 保留）。
- 只读态：`reader` 或非 holder 时输入框禁用 + 一行说明「接手后可继续」，不是发送后静默失败。
- 空会话不再靠前端伪造（`render.ts:527-538` 可删，保留兜底一行即可）——`session/new` 落库后
  列表里天然有它。

### 3.8 交接时「接着工作」必须同时成立的五件事

| # | 事实 | 现状 | 本设计 |
|---|---|---|---|
| 1 | 上下文连续 | 天然（同一 home） | 不变 |
| 2 | 做到哪一步 | `turn_checkpoints` | 共享空间**不自动续跑**，转 `waiting` 等人接手（见 §5.4） |
| 3 | 在哪个目录干活 | 不持久 | `sessions.cwd` 持久化，`turn/start` 缺省取它 |
| 4 | 交付物在哪 | 同一 home 的 `task_artifacts` | 不变 |
| 5 | 为什么这么做 | 同一 home 的 `facts`/`episodes` | 成为「团队记忆」，加入空间时明确告知 |

### 3.9 会话迁入（个人 → 工作空间）

**触发条件**（三条全部满足才出现按钮，否则按钮置灰并写明原因）：

1. 调用者是该会话的 `owner`；
2. 调用者是目标空间的 `writer` 或 `owner`（不是成员就先被邀请，**不设审批流**）；
3. 会话处于 `idle`：没有 running turn、没有 holder、没有等着的审批。

**执行方式：两步协议，不跨 home 直接读写。**

```
ops（编排，不做业务）
  ├─▶ 个人 app-server : session/export { sessionId }  ──▶ SessionBundle
  └─▶ 团队 app-server : session/import { bundle, cwd, visibility } ──▶ 新 sessionId
```

bundle 是自包含 JSON，先给人看一页预览（多少条消息、几个任务、几个文件、会写进谁的名字），
人点确认才投递。这一页预览不是装饰：它同时是「我到底交出去了什么」的唯一凭证。

| bundle 字段 | 内容 | 导入后的处理 |
|---|---|---|
| `session` | 标题、创建时间、owner | 标题可改，`origin` 记录来源 |
| `messages` | `role` / `content` / `actor` / `at` | 原样写入 `chat_log`，`actor` 保留原说话人 |
| `rollup` | 该会话的滚动摘要 | 作为新会话的初始上下文（压缩后的部分不失传） |
| `tasks` | 任务行 + 验收条件 + 交付物元数据 | 重建为 `suspended` + `needs_attention`，**由人决定是否继续** |
| `checkpoint` | **不导出** | 见 §1.3 第 3 条：结果未知的工具调用不能换人重放；上下文由 `rollup` + 一条「迁入说明」承接 |
| `files` | 引用清单：路径、字节数、sha256 | 见下方文件策略 |

**文件策略**（确认框里二选一，两项都必须显示清单）：

1. **只搬对话（默认）**：引用清单原样写进「迁入说明」——「以下文件仍只在 alice 的个人工作目录：
   …」。绝不静默丢引用，否则就是假完成。
2. **同机复制**（`copyFiles: true`）：把清单里真实存在的文件复制到目标 `cwd`，路径重写；
   同名不同内容**改名而不是覆盖**，并在说明里列出改名映射。
3. ~~什么都不做~~：不提供。没看过清单就不该迁移。

**幂等与失败**：`origin` 就是幂等键——同一个 bundle 重复导入返回已有会话，不产生第二条。
导入在一个事务里落库，中途失败则整条会话回滚，不会留下半个会话。

**迁移完成后**：

- 原会话：`moved_to = "team:joy/s456"`、`status = 'archived'`，列表里标「已迁至 团队 · 标题」，
  可读、不可再发消息（两个写入口的问题从根上消失）。
- 新会话：`visibility = 'space'`、`origin = "personal:alice/s123"`，首条消息是一条
  `system` 迁入说明（谁、从哪、搬了什么、哪些文件没搬、个人记忆未随行）。
- **记忆不随行**：个人 `facts`/`episodes` 留在个人空间；团队空间按自己的规则重新提炼。
  这一条必须写在确认框上，它是「个人记忆永不出个人空间」的唯一例外说明——例外的是**对话**，
  不是记忆。

---

## 4. 与基线的对齐（8 问，`基线 §9`）

| 问题 | 回答 |
|---|---|
| 提升了真实助理能力？ | 是：工作事项可以「交班」而不是「转述」，这正是长期委托的前提 |
| 改善自然交互？ | 是：不新增模式，仍是同一个对话窗口；只是空间不同 |
| 增强自主行动？ | 间接：接手让长任务能跨人跨时段继续，`needs_attention` 有真正的接收者 |
| 改善跨会话/跨进程连续性？ | 直接命中基线 §7 的「跨会话任务延续成功率」 |
| 增强安全与用户控制？ | 是：角色、可见性、租约、审计、写范围收敛到会话目录 |
| 复用了 Runtime？ | 是：只加一张元数据表与一层路由；Loop/Tools/Graph/Memory 一行不改 |
| 引入超需求复杂度？ | 风险点：团队空间是新增运维面。所以默认不建、单机单人零影响 |
| 可用真实任务评测验证？ | 是：`smoke-handover.sh` 与两条 eval 用例（见 §5.5） |

**反向约束（防止这套设计违反基线 2.2 / §6）**：工作会话不是工单。不引入状态机式审批流、
不引入「指派给谁」的任务队列、不要求用户先建空间再对话。默认路径永远是：打开就聊。

### 4.1 与审查五项优先级的关系（为什么用户中心不抢跑，也不排队）

| 审查优先级 | 关系 | 结论 |
|---|---|---|
| P0 统一助理自主性（Joy 自己建/改/结束长期事项） | 需要「以谁的身份、在哪个空间、花谁的钱」才能授权与归因 | **放在用户中心之后**；现在放开等于在团队空间里出现无人授权的任务 |
| P0 运行中交互与 Steering | 与接管**共用同一块地基**：会话级「一次只有一轮在跑」的门 + 入站消息的意图分类（steer / follow-up / context / 改目标） | **0.16 一起做**，两边都省一次改造 |
| 高 工具副作用恢复（执行账本 / 幂等键 / 未知结果策略） | 接管的**安全前置**：手上有一个结果未知的工具调用时，绝不能交给别人重放 | 0.18 先做「安全边界」版本：只允许在 idle 或等审批时交接；账本排在其后 |
| P1 个人与项目记忆 | 一个空间 home 就是一个项目域——`sessions.cwd` + 空间内存的 facts 天然是项目记忆的 key | **借用，不新建 crate** |
| P1 主动服务与通知可靠性 | 交接、`needs_attention`、租约到期都依赖「有人知道该接手了」 | 权威通道是驾驶舱的卡片与 `turn/list`（可轮询），外部通知只是加分项 |

一句话：**Steering 回答「我正在干活时你插话」，交接回答「我不在时换你说」——同一个门，
两种身份。**

---

## 5. 分阶段路线与验收

### 5.1 0.15 会话有名字（已完成）

- 迁移 0018，**只落最小列**：`sessions(id, title, cwd, created_at, last_at)`
  + 由 `chat_log` 反填旧会话（§6）。
  `owner / visibility / holder / lease_until / origin / moved_to` 与 `session_participants`、
  `chat_log.actor` 全部留到共享阶段用 `ALTER TABLE` 加——现在加只会得到一列恒为同一个值的空字段。
- `session/new` 落库、`session/get|update`、`session/list` join 元数据、`turn/list`。
- `sessions.cwd`：`turn/start` 未带 cwd 时取会话的；驾驶舱工作目录从 localStorage 的全局一个
  改成会话级一个。
- 验收：`smoke-dashboard.sh` 新增断言——空会话可命名并出现在列表；切会话后 `/api/turn`
  的 cwd 来自会话；`turn/list` 在跑时为 1、结束后为 0。
- 这一版**没有空间、没有共享**，但 `sessions` 表就是将来那些列的落点。

**落地形态（实际代码，比计划少一列）**：

- `0018_sessions.sql` 只有五列：`id / title / cwd / created_at / last_at`。`status` 也去掉了
  ——它要等 0.16 的 turn 门才有值可写，现在只会是恒为 `idle` 的空字段。
- 协议：`session/new`（`title?` `cwd?`）、`session/get`、`session/update`（`null` 不动 /
  `""` 清空）、`turn/list`；`SessionSummary` 多一个可选 `cwd`。
- ops：`GET /api/session/meta`、`POST /api/session/new`、`POST /api/session/update`、
  `GET /api/turns`。`/api/session` 仍只负责消息，两件事没混在一个路由里。
- 驾驶舱：新建会话走服务端拿 id（失败回退本地 id，首条消息照样落库）；切换会话时按
  `session/get` 的 `cwd` 设目录选择器；选目录时同时写回会话。
- 测试：状态层 3 条（空会话可见、改名/清空回退、幂等新建）、app-server 3 条
  （会话目录兜底、目录消失不退货、turn/list 只报在跑的）、`smoke-dashboard.sh` 6 组断言
  （含「不带 cwd 的一轮仍然落在会话目录」与「清空标题后回退首条消息」）。

### 5.2 0.16 会话门与运行中补充（已完成）

审查的 P0「运行中交互」与将来的「接管」是同一块地基，先把它单独做掉，到时候省一次改造。
它只依赖 0.15 的 `sessions` 表，不依赖任何空间概念。

- **会话级 turn 门**：同一 `session_id` 同一时刻只有一轮在跑。现有的 `session_locks`
  只护写段（`docs/limitations.zh.md:258-265`），这次护整轮——第二个 `turn/start` 拿到
  `SESSION_BUSY` 而不是并行跑。这一个门就是 0.18 里 `claim` 的实现底座。
- **运行中消息分四类**（由人发起，不是模型）：`steer`（注入当前轮的下一轮次）、
  `follow-up`（本轮结束后再跑一轮）、`context`（只进上下文）、`change-goal`（改目标）。
  第一版只实现 `steer` + `follow-up`，另外两种明确返回「未支持」，**不做静默降级成新 turn**。
- 验收：`evals/deterministic/steering.jsonl` —— mock 模型第二次调用前插入一条 steer，
  断言第二轮 prompt 含新指令、已完成的工具不重跑；同会话并发两次 `turn/start`，第二次被拒。
- 明确不做：把 steer 实现成「取消 + 重跑」（那会丢掉已完成的工具结果，正是审查指出的缺口）。

**落地形态（实际代码，比计划少 `follow-up`）**：

- 门就长在 `turns` 表上：`begin_turn` 在插入前先在表里找同会话的回合，找不到才插。没有第二份
  「谁占着会话」的状态可以漂移；`finish_turn` 删掉那一条，会话自然释放。认领点在任何副作用
  之前（任务心跳、checkpoint、计划生成都在它之后）。
- 续跑也过门：`resume_turn` 抢不到会话就把 checkpoint 留在 `open`，并说明「留到下次启动」。
- `turn/steer { sessionId, message }` → `{ turnId, queued }`；`Steering` 是「一个 Vec + 一个游标」，
  循环在轮次边界取未读部分，app-server 在收尾时读全量写日志（所以收尾前一刻补的话也不丢）。
- `chat_log` 的补句是独立 `user` 行 + `meta.steer`；`session_history` 合并连续 user 行，
  否则「最后一条 user 赢」会把原问题吃掉——这是这次改动里唯一的既有语义变更。
- 新通知 `turnSteered`；`joyczl-ops` 加 `POST /api/turn/steer`；驾驶舱运行中发送即补一句。
- 测试：loop 1 条（补句进第二次调用、工具不重跑、全量记录保留）、app-server 2 条
  （会话门、steering 顺序与「没在跑就说不」）、`smoke-dashboard.sh` 1 条
  （空闲时 steer 必须明说没有回合）。

**为什么 `follow-up` 挪后**：它要求「回合结束后由服务端起下一轮」，而现在一条 SSE 流只覆盖
一轮——服务端起的那轮没有任何客户端在听，等于把回复写进一个没人订阅的通道。要做得先把通知
路由改成按会话订阅，那是共享空间那一版的事。

> **以下 §5.3–§5.6 挂起**：设计保留，本轮不写代码。真正动手前先改 `reconcile`（§8）。四段之间
> 没有硬顺序，按当时的需要挑。

### 5.3 0.17 团队空间

- `users.json` v3 + `spaces` 账目 + `joy hub space add|rm|member`。
- `Hub::client_for` + `X-Joy-Space` + 成员校验 + 团队实例不按闲置驱逐。
- 驾驶舱空间切换、两段会话列表、参与者展示。
- 验收：`smoke-hub.sh` 两用户一个团队空间——A 建会话写消息，B 登录后能看到同一会话与
  同一批交付物；B 拿不到 A 个人空间的任何东西（404，不是空列表）。

### 5.4 0.18 接管

- `claim/release/handover` + 租约 + 驾驶舱接手按钮；`turn/start` 先 claim。
- **安全边界（先做这一条，账本可以晚）**：只在 `idle` 或 `waiting`（等审批）时允许交接；
  跑到一半的直接 `SESSION_BUSY`。理由见 §1.3 第 3 条——结果未知的工具调用不能换人重放。
- **`reconcile` 语义调整**：个人空间保持现状（无条件续跑）；团队空间把 open checkpoint
  转 `waiting` 并让驾驶舱亮 `needs_attention`，人接手后才续跑。外部通知只是加分项——
  它现在是 best-effort，不能作为交接的送达保证。
- 验收：`smoke-handover.sh` —— ① 跑到一半时 B 抢，被拒；② A 停下后 B 接手，`sessions.holder`
  变 B；③ `kill -9` A 的 app-server 再起，checkpoint 只被续跑**一次**；`chat_log.actor`
  依次是 A/B。

### 5.5 0.19 会话迁入（个人 → 工作空间）

- `session/export` / `session/import` 两个协议方法；ops 只做编排（个人侧取 bundle、团队侧投递），
  不做业务逻辑。`origin` 做幂等键，导入单事务。
- 驾驶舱：会话菜单「移到工作空间」+ 预览页（消息数 / 任务 / 文件清单 / 记忆不随行的提示）
  + 文件策略二选一（默认只搬对话，清单写进迁入说明）。
- 迁移只允许在 `idle` 发生；原会话 `moved_to` + `archived` 变只读，列表标「已迁至 …」。
- 验收：`smoke-promote.sh` —— 个人空间建会话、跑一轮并写一个文件 → 迁入团队空间 →
  断言 ① 团队侧消息条数与 `actor` 与原会话一致；② 任务变 `suspended` + `needs_attention`；
  ③ 迁入说明里列出未搬走的文件；④ 原会话只读且 `moved_to` 指向新会话；⑤ 同一 bundle 重复导入
  返回同一个 id；⑥ 迁入**没有**复制任何 `facts` 行。

### 5.6 0.20 共享的边界（安全面）

- 只读链接（短期 token，最小作用域，只能 `session/messages`）。
- 审计：`approval_log.session_id/actor` 进入 `/api/approvals`。
- 工具执行账本（审查的「高」优先级）：`tool_call_id` + 幂等键 + 未知结果的显式标记，
  让「重放」变成可选而不是默认。
- hub 已知短板补齐：登录限速与锁定、token 轮换/吊销、per-space 用量与配额（`docs/limitations.zh.md:37-44`）。
- eval：`evals/deterministic/shared-session.jsonl` —— 只读角色驱动被拒；租约过期可接手；
  写范围仍在会话 cwd 内（越界返回 `Error:` 文本而不是崩）。

---

## 6. 兼容与迁移

- **旧会话反填**：`INSERT INTO sessions(id, title, cwd, status, last_at)
  SELECT session_id, NULL, NULL, 'idle', MAX(created_at) FROM chat_log GROUP BY session_id;`
  `title` 留空 → 读取时回退首条用户消息，行为与今天一致。
- **双读一段时间**：`session/list` 用 `chat_log LEFT JOIN sessions`，没有行的按旧逻辑合成，
  保证「升级后旧会话不消失」。
- **默认零影响**：本轮没有空间概念——`users.json` 不动、hub 行为不变、`X-Joy-Space` 不存在。
- **破坏性**：无。0018 只加一张表；唯一的行为变化是 `session/new` 真的写一行。

---

## 7. 明确不做

1. 两人同时在同一会话里输入（实时协同）。单驱动是刻意的。
2. **把同一条会话挂到两个空间**（双写）。共享靠进入同一空间；「个人 → 工作」只在空闲时做
   一次显式迁入并封存原会话（§2.1、§3.9），不是持续同步。
3. 团队 → 个人的自动回流。要带走就是导出 + 人自己新建，不留自动通道（防团队记忆外流）。
4. 迁入时携带个人记忆。个人 `facts`/`episodes` 永不出个人空间；随行的是对话，不是记忆。
5. 会话级模型 / 密钥覆盖。模型是 home 级，避免「谁花钱」不可归因。
6. 工单化：指派、SLA、审批流、看板。工作会话仍是对话。

---

## 8. 风险与「顺序不能反」

| 风险 | 为什么危险 | 处理 |
|---|---|---|
| 先开放共享，后改 `reconcile` | 进程重启会替别人无条件续跑，产生无法归因的副作用 | 0.18 才开放接管；团队空间从 0.17 起就**不自动续跑** |
| 在工具跑到一半时交接 | 结果未知的工具调用会被下一个人重放（审查的副作用窗口） | 0.18 的安全边界：只在 idle / 等审批时交接 |
| 团队实例按闲置驱逐 | 驱逐 = 杀掉别人正在跑的 turn | 团队实例只在无活跃 checkpoint 时回收 |
| 会话 id 前端生成 | 两个成员可能撞 id，共享后直接串话 | `session/new` 服务端分配，前端只留离线草稿 |
| 共享空间的「记忆意外」 | 成员看到彼此在该空间的对话与文件 | 加入空间时明确告知 + 可见性默认 `space` 而非 `link` |
| 迁入把私人对话带进团队空间 | 进了团队 home 之后成员可见，事后删除也只是软删 | 预览页写明「谁会看到」；原会话在个人空间保留只读；空间 owner 可删 |
| 迁入后工作目录变了 | 交付物路径失效，任务的验收条件对不上 | 默认只搬对话并把未搬文件写进说明；任务降级 `needs_attention`，由人重定 cwd |
| 单机规模 | 一个团队空间 = 一个进程 = 串行执行（不同会话可并行） | 写进 limitations，不做横向扩展 |

---

## 9. 待办与挂起项

已定：工作会话 = 团队共享空间；个人会话可迁入 —— **两者都挂起**，先把一个会话做实。

**本轮只等你一句话**：是否从 0.15 开始动代码。它是一刀能切完的：

1. migration 0018（最小 `sessions` 表 + 反填旧会话）
2. `session/new` 落库、`session/get|update`、`session/list` join、`turn/list`
3. `turn/start` 缺省取 `sessions.cwd`；驾驶舱工作目录改成会话级
4. `smoke-dashboard.sh` 四条新断言

**挂起清单**（将来开工时按此顺序，先改 `reconcile`）：团队空间与角色 → 接管 → 迁入 →
只读链接/审计/工具账本。两个当时没定的小问题也一并留在这里：迁入默认搬不搬文件、接管要不要
「正在输入」的实时存在感。
