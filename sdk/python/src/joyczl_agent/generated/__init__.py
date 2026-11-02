"""生成物。**别手改。**

由 `scripts/write_schema.py --python` 从 Rust 的 `joyczl-protocol` 生成：

- `v2.py` —— 63 个类型，datamodel-codegen 从 `schema/json/v2.json` 出的 pydantic 模型；
- `consts.py` —— 两组常量，从 `schema/json/consts.json` 出的 `CODES` / `METHODS`。

包里的别处一律从 `joyczl_agent.protocol` 引，不要直接引这里 —— 「生成物躺在哪」
这件事整个包里只该有一处知道，那样挪目录才只用改一个文件。
"""
