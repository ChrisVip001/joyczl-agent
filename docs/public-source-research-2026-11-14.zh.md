# 公开资料研究试验：工作区选择与文件访问权限

[English](public-source-research-2026-11-14.md) | 简体中文

2026-11-14 使用 Joy 和现有 ds/deepseek-flash 配置观察。这是两份指定官方资料的有限研究，不是自主文献检索，也不是对 DeepSeek harness 本身的性能评测。

Web UI 需要先选择工作区，输入框才可使用。这是界面前置条件，不能据此推断文件系统隔离。Python SDK 文档将 `sdk-minimal` 描述为使用 `danger-full-access`，因此该配置的工作目录不能当作文件系统权限边界。结论只针对观察时文档描述的配置，不能推广到所有集成和未来版本。

| 发现 | 官方来源 | 保留证据 |
|---|---|---|
| Web UI 输入需要先选择工作区 | [快速开始](https://deepseek-harness.github.io/deepseek-harness/guide/quickstart) | 原文短引、读取时间和页面字节摘要 |
| 最小 SDK 配置具有较广文件访问权限 | [Python SDK](https://deepseek-harness.github.io/deepseek-harness/guide/python-sdk) | 原文短引、读取时间和页面字节摘要 |

[正向原始记录](../evals/results/deepseek-flash-2026-11-14-research.json)包含独立来源回执和实际产物摘要。[JSON 语义副本](../evals/results/public-source-research-2026-11-14.json)由保留结果重新序列化，字节并非原临时文件；其中的原产物摘要不能用来校验副本字节。没有归档完整网页。

故意让第二份来源不可用后，发现一个失败：任务保持 `needs_attention`，但模型仍违背指令写出了部分最终文件。[失败记录](../evals/results/deepseek-flash-2026-11-14-research-unavailable.json)予以保留。最终验收避免了虚假的 `done`，却没有阻止不完整文件的生成。因此增加独立的来源校验发布工具，并在受控试验中去掉通用文件写入能力：两次来源读取、精确引文、布尔事实和输出结构全部通过后，才原子发布到固定路径。这是评测器内的领域适配器，不代表任意 Joy 文件工具都新增了该保证。

逐字引文与类型校验不能评估任意综合分析的质量，也不能发现全部隐含过度推断。本试验只有两项预定义事实、短摘要、固定来源和本地产物，是范围有限的真实联网闭环。长研究、自主发现、编辑质量、跨日托管和真实外部事务交付仍未验证。
