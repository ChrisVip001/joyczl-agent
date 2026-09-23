"""协议里的两组常量。**生成物，别手改。**

由 `scripts/write_schema.py` 从 `schema/json/consts.json` 生成，而那份 JSON
由 Rust 的 `joyczl-protocol`（`rpc::codes`、`protocol::v2::methods`）导出。

TS 侧同一份东西在 `schema/typescript/v2/{codes,methods}.mts` —— 同一个源头。
**所以「memory/remember」这个字符串全仓库只有一处定义**，三种语言都从它生成。
"""

from __future__ import annotations

from enum import Enum, IntEnum


class Codes(IntEnum):
    """错误码。名字跟 Rust 侧常量同名，好让两边 grep 得到同一处。"""

    INTERNAL_ERROR = -32603
    INVALID_PARAMS = -32602
    INVALID_REQUEST = -32600
    METHOD_NOT_FOUND = -32601
    NOT_IMPLEMENTED = -32002
    PARSE_ERROR = -32700
    PROVIDER_ERROR = -32000
    TOOL_ERROR = -32001


class Methods(str, Enum):
    """方法名。值就是线上传的字符串。"""

    # 3.10 没有 StrEnum；`str, Enum` 这手在 f-string 里会打出
    # "Methods.TURN_START"，而方法名是要拼进日志和报错里的 —— 换回来。
    __str__ = str.__str__

    APPROVAL_RESPOND = "approval/respond"
    CONFIG_READ = "config/read"
    CONFIG_WRITE = "config/write"
    GOAL_SET = "goal/set"
    MEMORY_FORGET = "memory/forget"
    MEMORY_LIST = "memory/list"
    MEMORY_LIST_EPISODES = "memory/list-episodes"
    MEMORY_REMEMBER = "memory/remember"
    MEMORY_SEARCH = "memory/search"
    MODEL_LIST = "model/list"
    SESSION_LIST = "session/list"
    SESSION_MESSAGES = "session/messages"
    SESSION_NEW = "session/new"
    TURN_INTERRUPT = "turn/interrupt"
    TURN_START = "turn/start"
