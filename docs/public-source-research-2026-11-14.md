# Public-source research pilot: workspace selection and filesystem access

English | [简体中文](public-source-research-2026-11-14.zh.md)

Observed on 2026-11-14 with Joy and the existing ds/deepseek-flash profile. This is a bounded study of two supplied official sources, not an autonomous literature review or a benchmark of the DeepSeek harness itself.

The Web UI requires selecting a workspace before its input becomes available. This describes a UI prerequisite; it does not establish filesystem isolation. The Python SDK documentation describes `sdk-minimal` as using `danger-full-access`. A working directory therefore must not be treated as a filesystem boundary for that profile. These findings apply to the documented profiles at the observation time, not every integration or future version.

| Finding | Official source | Retained evidence |
|---|---|---|
| Web UI input requires workspace selection | [Quickstart](https://deepseek-harness.github.io/deepseek-harness/guide/quickstart) | Literal quotation, retrieval time and source-byte digest |
| Minimal SDK profile has broad filesystem access | [Python SDK](https://deepseek-harness.github.io/deepseek-harness/guide/python-sdk) | Literal quotation, retrieval time and source-byte digest |

The [positive raw run](../evals/results/deepseek-flash-2026-11-14-research.json) includes independent source receipts and the observed final artifact digest. The [semantic JSON copy](../evals/results/public-source-research-2026-11-14.json) was re-serialized from the retained result: its bytes are not the original temporary artifact, and its original-artifact digest must not be used as the copy's digest. Full web pages were not archived.

A deliberately unavailable second source exposed a failure: the task remained `needs_attention`, but the model still wrote a partial final file despite instructions forbidding it. The [failed run](../evals/results/deepseek-flash-2026-11-14-research-unavailable.json) is retained. Final task acceptance prevented a false `done`; it did not prevent creation of an incomplete file. This motivated a separate evidence-gated publication tool with no generic file-write capability in the guarded pilot. It validates both observed source reads, exact citations, typed facts and output shape before atomically publishing a fixed path. This is a domain adapter in the evaluator, not a new guarantee for arbitrary Joy file tools.

Literal citation checks and typed facts cannot evaluate arbitrary synthesis quality or detect every unsupported implication. The two predefined facts, short summary, fixed sources and local artifact make this a narrow real-network workflow. Long research, independent discovery, editorial quality, cross-day operation and external transaction delivery remain unvalidated.
