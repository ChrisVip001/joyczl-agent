"""The one place in the whole repo that points at the Python artefacts.

Everything else imports from `joyczl_agent.protocol`, so "where the artefacts lie" is known
in exactly one place in the package — move a directory, or change how they are packaged,
and only this file changes.

The same role as `packages/client/src/protocol.ts` on the TS side, except that file is one
line of `export type * from ...`. **Here it has to be a hand-written list, which is not
laziness but Python's static types failing to follow a computed re-export**:
`__all__ = sorted({...})` is perfect at runtime, and mypy cannot evaluate it, so
`from .protocol import *` in `__init__.py` imports nothing as far as mypy is concerned —
and a user writing `from joyczl_agent import Methods` is told there is no such attribute.
For an SDK whose selling point is its annotations, that is worse than a drifted list.

The cost of the list drifting is covered by
`tests/test_client.py::test_every_method_type_is_exported`: Rust adds a method, the
artefacts follow, this is not updated, and that test goes red.
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
    MemoryValidityParams,
    MemoryValidityResponse,
    MemoryProvenanceParams,
    MemoryProvenanceResponse,
    MemoryObservation,
    MemoryRememberParams,
    MemoryRememberResponse,
    MemorySearchParams,
    MemorySearchResponse,
    Message,
    MessageRole,
    ModelInfo,
    ModelListParams,
    ModelListResponse,
    ModelProfilesParams,
    ModelProfilesResponse,
    ModelProfileView,
    ModelCapabilities,
    ModelRemoveParams,
    ModelRemoveResponse,
    ModelRoutesResponse,
    ModelRoutesSaveParams,
    TierRoute,
    RunResponse,
    InboxListView,
    InboxSummarizeParams,
    InboxSummarizeResponse,
    ScheduleListParams,
ScheduleListResponse,
ScheduleStateRow,
ApprovalLogEntry,
ApprovalLogParams,
ApprovalLogResponse,
UsageByModel,
    UsageSummaryParams,
    UsageSummaryResponse,
    ModelSaveParams,
    ModelSaveResponse,
    ServerNotification,
    SessionListParams,
    SessionListResponse,
    SessionMessagesParams,
    SessionMessagesResponse,
    SessionNewParams,
    SessionNewResponse,
    SessionGetParams,
    SessionGetResponse,
    SessionUpdateParams,
    SessionUpdateResponse,
    SessionSummary,
    SettingsPatch,
    SettingsView,
    TokenUsage,
    ToolCallRecord,
    ToolStatus,
    ApprovalRespondParams,
    ApprovalRespondResponse,
    SessionViewParams,
    TaskCreateParams,
    TaskStartParams,
    TaskOperationsResponse,
    TaskReconcileParams,
    TurnOperationsParams,
    TurnReconcileParams,
    ToolOperationView,
    TaskWaitParams,
    TaskSignalParams,
    TaskSignalResponse,
    TaskInputView,
    TaskListParams,
    TaskActionParams,
    TaskEnvelope,
    TaskListResponse,
    TaskViewResponse,
    TaskGetParams,
    TaskArtifactsResponse,
    TaskRunsResponse,
    AgentTaskView,
    TodoListView,
    TurnInterruptParams,
    TurnInterruptResponse,
    TurnListParams,
    TurnListResponse,
    TurnSteerParams,
    TurnSteerResponse,
    TurnFollowUpParams,
    TurnFollowUpResponse,
    TurnMeta,
    TurnStartParams,
    TurnStartResponse,
    # The memory center's windows, manual compaction, session forking: all methods added in
    # 0.16–0.17. The artefacts had them long ago; this list never followed
    # (`test_every_method_type_reaches_the_package` was red).
    ContextUsageParams,
    ContextUsageResponse,
    MemoryIndexResponse,
    MemoryLedgerParams,
    MemoryLedgerResponse,
    MemoryPageParams,
    MemoryPageResponse,
    SessionCompactParams,
    SessionCompactResponse,
    SessionForkParams,
    SessionForkResponse,
    SkillStatsResponse,
    RememberScope,
)

#: The notification union, plucked off the generated `RootModel`.
#:
#: **The 12 names are not copied by hand**: done by hand, a notification type added in Rust
#: would quietly leave a missing arm here, and that is exactly what this project spends its
#: time eliminating. `model_fields["root"]` is that `A | B | ...`.
#:
#: The cost, stated plainly: mypy cannot evaluate this expression, so `Notification` is `Any`
#: to it. No static narrowing, and `n.delta` passes the check by not being checked. Runtime
#: is right — the discriminant `type` is a Literal, and pydantic picks the correct arm.
Notification = ServerNotification.model_fields["root"].annotation

#: Notifications all travel under this method name on the wire; the real discriminant is
#: `params.type`.
NOTIFICATION_METHOD = "turn/notification"

# ── About those 12 named notification classes (`TextDeltaNotification` and kin) ──────
#
# They exist in the artefacts but are **deliberately not exported from here**: the variant
# inside the union is another copy that schemars inlined, and the two are not the same
# class — `isinstance(n, TextDeltaNotification)` would be False. Exporting them hands out
# a trap.
#
# The root cause sits in the JSON Schema: for an internally tagged enum
# (`#[serde(tag = "type")]`) schemars can only flatten the payload into `oneOf`, so the
# named definition and the inlined copy each get generated. TS does not have this problem —
# it is a composition generated by ts-rs
# (`{ type: "turnStarted" } & TurnStartedNotification`). Removing it means rewriting the
# inlined variants as `allOf: [$ref]` when exporting the JSON Schema, which is Rust's
# business, not this package's.
#
# So the discriminant is taken as `n.type == "textDelta"` — the same spelling as
# `if (n.type === "textDelta")` on the TS side.

__all__ = [
    "NOTIFICATION_METHOD",
    "Codes",
    "Methods",
    "Notification",
    "ServerNotification",
    # Arguments and responses: one pair per method.
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
    "MemoryValidityParams",
    "MemoryValidityResponse",
    "MemoryProvenanceParams",
    "MemoryProvenanceResponse",
    "MemoryObservation",
    "MemoryRememberParams",
    "MemoryRememberResponse",
    "MemorySearchParams",
    "MemorySearchResponse",
    "ModelListParams",
    "ModelListResponse",
    "ModelProfilesParams",
    "ModelProfilesResponse",
    "ModelProfileView",
    "ModelCapabilities",
    "ModelRemoveParams",
    "ModelRemoveResponse",
    "ModelRoutesResponse",
    "ModelRoutesSaveParams",
    "TierRoute",
    "RunResponse",
    "InboxListView",
    "InboxSummarizeParams",
    "InboxSummarizeResponse",
    "ScheduleListParams",
    "ScheduleListResponse",
    "ScheduleStateRow",
    "ApprovalLogEntry",
    "ApprovalLogParams",
    "ApprovalLogResponse",
    "UsageByModel",
    "UsageSummaryParams",
    "UsageSummaryResponse",
    "ModelSaveParams",
    "ModelSaveResponse",
    "SessionListParams",
    "SessionListResponse",
    "SessionMessagesParams",
    "SessionMessagesResponse",
    "SessionNewParams",
    "SessionNewResponse",
    "SessionGetParams",
    "SessionGetResponse",
    "SessionUpdateParams",
    "SessionUpdateResponse",
    "ApprovalRespondParams",
    "ApprovalRespondResponse",
    "SessionViewParams",
    "TaskCreateParams",
    "TaskStartParams",
    "TaskOperationsResponse",
    "TaskReconcileParams",
    "TurnOperationsParams",
    "TurnReconcileParams",
    "ToolOperationView",
    "TaskWaitParams",
    "TaskSignalParams",
    "TaskSignalResponse",
    "TaskInputView",
    "TaskListParams",
    "TaskActionParams",
    "TaskEnvelope",
    "TaskListResponse",
    "TaskViewResponse",
    "TaskGetParams",
    "TaskArtifactsResponse",
    "TaskRunsResponse",
    "AgentTaskView",
    "TodoListView",
    "TurnInterruptParams",
    "TurnInterruptResponse",
    "TurnListParams",
    "TurnListResponse",
    "TurnSteerParams",
    "TurnSteerResponse",
    "TurnFollowUpParams",
    "TurnFollowUpResponse",
    "TurnStartParams",
    "TurnStartResponse",
    # Memory center / compaction / forking
    "ContextUsageParams",
    "ContextUsageResponse",
    "MemoryIndexResponse",
    "MemoryLedgerParams",
    "MemoryLedgerResponse",
    "MemoryPageParams",
    "MemoryPageResponse",
    "SessionCompactParams",
    "SessionCompactResponse",
    "SessionForkParams",
    "SessionForkResponse",
    "SkillStatsResponse",
    "RememberScope",
    # The types **inside** responses and notifications. Without these a user can reach
    # `reply.facts[0]` but cannot write `def show(fact: Fact)` for their own function.
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
