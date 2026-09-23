"""全仓库唯一指向 Python 生成物的地方。

别的文件一律从 `joyczl_agent.protocol` 引，这样「生成物躺在哪」这件事整个包里
只有一处知道 —— 挪目录、或者哪天生成物换个装法，只需要改这一个文件。

跟 TS 侧的 `packages/client/src/protocol.ts` 是同一个角色，但那文件里是一行
`export type * from ...`。**这里只能是一份手写清单，不是偷懒，是 Python 的静态
类型跟不上算出来的再导出**：`__all__ = sorted({...})` 这种在运行时完美，而 mypy
无法求值，于是 `__init__.py` 的 `from .protocol import *` 在它眼里一个名字都没
导入 —— 用户写 `from joyczl_agent import Methods` 会直接报「没有这个属性」。
对一个卖点就是标注的 SDK，那比清单漂了更糟。

清单漂了的代价由 `tests/test_client.py::test_every_method_type_is_exported` 兜着：
Rust 加了方法、生成物跟上了、这里忘了补，那条测试会红。
"""

from __future__ import annotations

from .generated.consts import Codes, Methods
from .generated.v2 import (
    ConfigReadParams,
    ConfigReadResponse,
    ConfigWriteParams,
    ConfigWriteResponse,
    Episode,
    ErrorObject,
    Fact,
    GateDecision,
    GateDecisionKind,
    GraphRouteKind,
    MemoryForgetParams,
    MemoryForgetResponse,
    MemoryListEpisodesParams,
    MemoryListEpisodesResponse,
    MemoryListParams,
    MemoryListResponse,
    MemoryRememberParams,
    MemoryRememberResponse,
    MemorySearchParams,
    MemorySearchResponse,
    Message,
    MessageRole,
    ModelInfo,
    ModelListParams,
    ModelListResponse,
    ServerNotification,
    SessionListParams,
    SessionListResponse,
    SessionMessagesParams,
    SessionMessagesResponse,
    SessionNewParams,
    SessionNewResponse,
    SessionSummary,
    SettingsPatch,
    SettingsView,
    TokenUsage,
    ToolCallRecord,
    ToolStatus,
    ApprovalRespondParams,
    ApprovalRespondResponse,
    GoalSetParams,
    GoalSetResponse,
    TurnInterruptParams,
    TurnInterruptResponse,
    TurnMeta,
    TurnStartParams,
    TurnStartResponse,
)

#: 通知的联合类型，从生成的 `RootModel` 上摘下来。
#:
#: **不手抄那 12 个名字**：手抄的话，Rust 那边加一个通知类型，这边会安静地少一支，
#: 而那正是这个工程到处在消灭的东西。`model_fields["root"]` 就是那个 `A | B | ...`。
#:
#: 代价要说清楚：mypy 求不出这个表达式，`Notification` 在它眼里是 `Any`。所以
#: 静态窄化没有，`n.delta` 过得了检查但不被检查。运行时是对的 —— 判别式 `type`
#: 是 Literal，pydantic 会挑中正确的那一支。
Notification = ServerNotification.model_fields["root"].annotation

#: 通知在线上统一挂在这个方法名底下，真正的判别式在 `params.type`。
NOTIFICATION_METHOD = "turn/notification"

# ── 关于那 12 个具名通知类（`TextDeltaNotification` 等）─────────────────────
#
# 生成物里它们存在，但**故意不从这里出去**：联合里的变体是 schemars 内联出来的
# 另一份副本，两边不是同一个类 —— `isinstance(n, TextDeltaNotification)` 会是
# False。导出它们等于发一个陷阱。
#
# 根因在 JSON Schema 侧：schemars 对内部标记枚举（`#[serde(tag = "type")]`）只能
# 把载荷摊平进 `oneOf`，于是具名定义和内联副本各生成一套。TS 侧没有这问题，它
# 是 ts-rs 生成的组合写法（`{ type: "turnStarted" } & TurnStartedNotification`）。
# 要消掉得在导出 JSON Schema 时把内联变体改写成 `allOf: [$ref]`，那是 Rust 侧的
# 事，跟这个包无关。
#
# 所以取判别式的方式是 `n.type == "textDelta"` —— 跟 TS 那边
# `if (n.type === "textDelta")` 是同一个写法。

__all__ = [
    "NOTIFICATION_METHOD",
    "Codes",
    "Methods",
    "Notification",
    "ServerNotification",
    # 参数与应答：一个方法一对。
    "ConfigReadParams",
    "ConfigReadResponse",
    "ConfigWriteParams",
    "ConfigWriteResponse",
    "MemoryForgetParams",
    "MemoryForgetResponse",
    "MemoryListEpisodesParams",
    "MemoryListEpisodesResponse",
    "MemoryListParams",
    "MemoryListResponse",
    "MemoryRememberParams",
    "MemoryRememberResponse",
    "MemorySearchParams",
    "MemorySearchResponse",
    "ModelListParams",
    "ModelListResponse",
    "SessionListParams",
    "SessionListResponse",
    "SessionMessagesParams",
    "SessionMessagesResponse",
    "SessionNewParams",
    "SessionNewResponse",
    "ApprovalRespondParams",
    "ApprovalRespondResponse",
    "GoalSetParams",
    "GoalSetResponse",
    "TurnInterruptParams",
    "TurnInterruptResponse",
    "TurnStartParams",
    "TurnStartResponse",
    # 应答和通知**里面**的那些类型。不给出来的话，用户能拿到
    # `reply.facts[0]`，却没法给自己的函数写 `def show(fact: Fact)`。
    "Episode",
    "ErrorObject",
    "Fact",
    "GateDecision",
    "GateDecisionKind",
    "GraphRouteKind",
    "Message",
    "MessageRole",
    "ModelInfo",
    "SessionSummary",
    "SettingsPatch",
    "SettingsView",
    "TokenUsage",
    "ToolCallRecord",
    "ToolStatus",
    "TurnMeta",
]
