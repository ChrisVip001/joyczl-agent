# 贡献指南

[English](CONTRIBUTING.md) | 简体中文

## 开发环境

```bash
cargo build                     # Rust ≥ 1.85
just ts-install && just py-install
```

## 工程纪律（合并前必须全绿）

```bash
just check
```

等价于依次执行：`cargo fmt --check` → 生成物漂移检查 → `just ts-check` →
`just py-check` → `clippy -D warnings` → `cargo test --workspace` →
`joy eval`（release gate）。

## 改动的黄金规则

1. **改了协议必须重新生成**：`just write-app-server-schema`，生成物
   check-in，漂移会被 `check-app-server-schema` 在 CI 拦下。
2. **clippy 零告警**：`-D warnings` 是门禁，不是建议。
3. **行为改动配测试**：修 bug 在复现它的测试旁写回归；新行为至少一条
   确定性测试。测试放 crate 内（单元）或 `evals/deterministic/`
   （跨模块组装）。
4. **工具错误是文本不是异常**：`ToolRegistry::execute` 永不 Err。
5. **失败开放的地方要有测试钉住**：检索门、triage、gather 的失败路径
   与成功路径同等重要。
6. **不新增重型依赖**：先看工作区里有没有等价物；URL 编解码这类小工具
   优先复用现有实现。

## 提交

一次提交一件事；提交信息用中文一句话概括行为变化（不是文件清单）。
`just check` 全绿再推。
