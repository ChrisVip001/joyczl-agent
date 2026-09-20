# Contributing

English | [简体中文](CONTRIBUTING.zh.md)

## Development environment

```bash
cargo build                     # Rust >= 1.85
just ts-install && just py-install
```

## Engineering discipline (all green before merge)

```bash
just check
```

Equivalent to, in order: `cargo fmt --check` → generated-artifact drift check
→ `just ts-check` → `just py-check` → `clippy -D warnings` →
`cargo test --workspace` → `joy eval` (the release gate).

## Golden rules for changes

1. **Protocol changes must be regenerated**: run
   `just write-app-server-schema` and commit the artifacts; the CI drift
   check catches anything forgotten.
2. **Zero clippy warnings**: `-D warnings` is a gate, not a suggestion.
3. **Behavior changes come with tests**: fix a bug next to the test that
   reproduces it; new behavior gets at least one deterministic test. Unit
   tests live in the crate; cross-module assembly behavior lives in
   `evals/deterministic/`.
4. **Tool errors are text, not exceptions**: `ToolRegistry::execute` never
   returns Err.
5. **Fail-open paths need pinned tests**: the failure paths of the retrieval
   gate, triage, and gather matter as much as the success paths.
6. **No heavy new dependencies**: check the workspace for an equivalent first
   (URL codecs, hashing, and similar small utilities already exist here).

## Commits

One commit, one concern; commit messages summarize the behavior change in one
sentence (not a file list). Run `just check` until green before pushing.
