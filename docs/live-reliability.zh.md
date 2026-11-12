# 限额真实模型可靠性冒烟评测

[English](live-reliability.md) | 简体中文

2026-11-13 使用现有配置 `ds`：服务商 `deepseek`，模型别名 `deepseek-flash`。这是临时隔离环境中的合成样本，不代表生产可靠性。未使用个人聊天历史、外部搜索、消息发送或真实事务。

## 结果

| 场景 | 复核结果 | 证据 |
|---|---|---|
| 中文聊天连续性 | 通过 | 两轮对话后记得猫名 Momo 和项目汇报焦虑 |
| 文件交付 | 通过 | 写入三项要求、调用工具读回、文件验收通过，任务为 `done` |
| 外部操作结果未知 | 通过 | 本地 MCP 服务提交后断线；提交次数恰为一次，任务为 `needs_attention`，没有重放或虚报完成 |

每个场景运行两次。首轮评测器误将状态断言写成 `needsAttention`，实际运行时返回 `needs_attention`，安全行为正确。保留首轮失败记录；修正评测器后，三个复核样本均通过。样本很少，不能据此推导通用成功率。真实模型套件尚未测量重启恢复、长研究质量、情绪支持质量或交付可靠性；相应离线回归有独立覆盖。

两轮共 20 次逻辑模型请求，无缺失用量的失败请求。返回用量合计：输入 15,009 tokens，输出 4,157 tokens。按官方高峰缓存未命中价格（输入 ¥2/百万、输出 ¥8/百万）估算 **¥0.063274**；两轮保守请求预留合计 **¥1.523568**，低于用户授权总额 **¥10**。未扣缓存与非高峰优惠。这是按用量估算，不是账单核销。[官方价格](https://api-docs.deepseek.com/zh-cn/quick_start/pricing/)。

每次请求最多输出 2,048 tokens，序列化输入不超过 64,000 字节，另加预留余量；每轮评测最多 18 次调用，关闭 provider 自动重试。发送前按高峰上界的两倍预留费用。检索门等全部模型请求共用限额器与主力模型。多轮评测共同遵守总预算：首轮预留 ¥0.761848，第二轮上限下调为 ¥9。

## 显式复现

```sh
cargo test -p joyczl-eval --example live_reliability --manifest-path joy-rs/Cargo.toml
cargo run -p joyczl-eval --example live_reliability --manifest-path joy-rs/Cargo.toml -- \
  --home /path/to/configured/joy-home --budget-cny 1 --out /tmp/joy-live.json
```

真实评测命令会连接已配置的官方 DeepSeek 服务并产生费用。只接受已知费率的 `deepseek-flash`，后续运行前应核对价格。通过已有 resolver 在内存中读取凭据；每个用例使用独立临时状态和工具白名单。此套件刻意不纳入 `just check`。

原始证据：[首轮](../evals/results/deepseek-flash-2026-11-13-initial.json)、[复核](../evals/results/deepseek-flash-2026-11-13-confirmed.json)。记录中的临时路径属于已销毁的测试工作区，不是保留的交付物。
