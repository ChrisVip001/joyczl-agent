# 限额真实模型可靠性冒烟评测

[English](live-reliability.md) | 简体中文

2026-11-13 使用现有配置 `ds`：服务商 `deepseek`，模型别名 `deepseek-flash`。这是临时隔离环境中的合成样本，不代表生产可靠性。未使用个人聊天历史、外部搜索、消息发送或真实事务。

## 结果

| 场景 | 复核结果 | 证据 |
|---|---|---|
| 对话连续性 | 通过 | 两轮对话后记得猫名 Momo 和项目汇报焦虑 |
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
## 扩展生命周期样本

扩展运行增加了明确的中文连续性、等待输入后交付和过期记忆召回场景。中文连续性和过期记忆
召回通过。首次等待样本用尽评测器的四步额度，正确停在 `needs_attention`，未宣称核验完成；
原始失败记录保留。仅复测等待场景时提高到八步，保留 token、请求次数和费用限制，
任务通过有类型的 JSON 验收并达到 `done`。

复测的原始自动判定仍为 false：计数器把草稿 `meeting.draft.json` 和最终文件 `meeting.json`
各一次写入合并成了重复最终写入。保留的操作回执显示两者各一次、同一个 run、输入前没有最终文件、
聊天独立完成、重复事件被确认，以及数值 `approved:42`。带来源摘要的离线证据复核确认运行时结果；
修正后的评测器仅统计最终路径，并有回归测试。这是证据复核，不是另一次成功的模型运行。
旧记录没有单独保留首次输入的确认响应。情绪支持质量、长研究质量和真实跨日恢复仍未测量。

四次运行累计 **45 次逻辑请求**、**输入 58,534 / 输出 10,122 tokens**，没有记录到缺失用量的
失败请求。高峰价格用量估算 **¥0.198044**，保守预留合计 **¥3.819316**，在授权总额 **¥10** 内。
调用前已复核价格，未扣优惠；仍是估算，不是账单核销。

证据：[扩展原始运行](../evals/results/deepseek-flash-2026-11-13-expanded.json)、
[定向复测](../evals/results/deepseek-flash-2026-11-13-wait-confirmed.json)、
[带来源摘要的离线复核](../evals/results/analysis/wait-delivery-review.json)。
显式运行扩展套件使用 `--suite expanded`，可通过 `--case waiting-chat-delivery` 选择场景；
多次运行必须共享总预算。

## 2026-11-14 临时状态提炼样本

新增 `--suite memory`，使用同一 `ds / deepseek-flash` 配置、独立临时状态和单次 ¥0.50 上限。
合成输入只要求把称呼“小林”保存为长期事实，并将当天疲惫作为临时状态。一次模型调用通过：
只有一条长期称呼事实；疲惫带有匹配原文、来源摘要、6 小时有效期与模型自报置信度 0.9。
有效期由原消息时间计算。置信度未经校准，结果不代表情绪支持质量或普遍语义分类正确率。
评测器同时有离线反例：即使生成临时副本，只要又生成额外的长期情绪事实仍判失败。

本次输入 440 / 输出 345 tokens，高峰价格估算 ¥0.003640，保守预留 ¥0.072180，无缺失用量请求。
五次程序运行累计 **46 次请求、输入 58,974 / 输出 10,467 tokens、估算 ¥0.201684、保守预留 ¥3.891496**，
仍在原 ¥10 授权总额内。价格于 2026-11-14 复核，沿用高峰上界，不扣优惠，不是账单核销。
[官方价格](https://api-docs.deepseek.com/zh-cn/quick_start/pricing/)。

[原始输入、模型文本与数据库结果](../evals/results/deepseek-flash-2026-11-14-memory.json)；
[累计预算记录](../evals/results/live-budget-2026-11-14.json)。显式复现命令在原命令上增加 `--suite memory`；
它会产生费用，多次运行须共享总预算。原有 baseline/expanded 套件没有增加隐式调用。

## 2026-11-14 固定来源研究试验

`--suite research --case source-report` 通过两份指定官方页面的真实 HTTPS 读取、独立回执、类型事实和 JSON 交付。单独的 `--case source-unavailable` 失败：任务保持 `needs_attention`，但模型仍写了部分最终文件，说明最终验收本身无法阻止发布。原始失败保留，没有改为通过。

`--suite research-guarded` 去掉通用文件写入，改用固定路径的 MCP 发布工具。必须已有两份进程内实际读取、精确引文、真正布尔类型事实、短摘要和幂等输出校验，才原子发布。示例七项测试及 Clippy 通过，覆盖缺失/部分读取、错误值/类型和不同内容覆盖。真实正反样本均通过：来源齐全时 `done`；缺失时无最终文件并停在 `needs_attention`。模型仍多次请求失败来源，因此没有证明高效失败即停或错误说明文案质量。这是显式启用评测器中的领域适配器，不是生产通用文件写入政策，也不是任意综合分析的质量判定器。

复现时在已有明确 home/profile 的命令中加入 `--suite research-guarded --budget-cny 2 --out <新记录路径>`。会产生费用，每次调用前需检查共享累计上限，使用新记录路径保留原始结果。本试验指定来源与事实，不评自主搜索、长研究、跨日运行，也不评页面所描述外部 harness 的运行性能。

累计八次调用：**72 次请求，输入 105,485 / 输出 16,688 tokens，高峰价估算 ¥0.344474，保守预留 ¥6.274340**，低于共享 ¥10 上限。记录请求均有 usage。估算与预留不是账单。一次未注册 suite 的本地启动失败发生在模型请求之前，不计为付费调用；价格于 2026-11-14 核查。

[正向原始记录](../evals/results/deepseek-flash-2026-11-14-research.json)、[保留的反向失败](../evals/results/deepseek-flash-2026-11-14-research-unavailable.json)、[受控发布正反样本](../evals/results/deepseek-flash-2026-11-14-research-guarded.json)、[验证摘要](../evals/results/research-validation-2026-11-14.json)、[累计费用](../evals/results/live-budget-2026-11-14.json)、[研究报告](public-source-research-2026-11-14.zh.md)。
