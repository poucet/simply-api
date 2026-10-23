# The sense copy of simply-api and friends, against flux

Compared on 2026-10-23 (change `orlnwpsz`). Sources: sense main `9644bfff`
(`crates/simply-{api,api-macros,history,op-macros}`, introduced in sense's
first commit `tqkwyqnv`, 2026-09-03) and flux main `bae34fcf`, which is what
this repo was extracted from. Method: `git archive` of both, `diff -ru`. Every
`-` hunk (a line sense has that flux does not) was read.

**Result: sense has nothing flux lacks.** sense's copy is flux as of early
September. Of the 15 files that differ (496 lines added on flux's side, 96
removed), every difference falls into one of four groups:

| what | where | which side has it |
|---|---|---|
| Formatting: sense was formatted with rustfmt's default `fn_call_width = 60`. flux's `rustfmt.toml` sets `use_small_heuristics = "Max"` | `simply-history` tree.rs and tests, `simply-op-macros` lib.rs and tests, `simply-api` tests/derive.rs, `simply-api-macros` lib.rs | neither; formatting only |
| Path deps with `version = "0.1.0"` instead of `.workspace = true` | both `Cargo.toml`s | sense, as packaging only |
| Doc pointers to `docs/research/flux-api-notes.md`, a note that exists in neither repo now | `simply-api` lib.rs, `simply-api-macros` lib.rs and parse.rs | sense (flux dropped the path and left the sentence dangling, which this repo closes) |
| Features | below | flux only |

Features flux has and sense lacks:

- `#![forbid(unsafe_code)]` in all four crates.
- `MethodMeta.help` (the whole doc comment), `tier` (`#[api(tier = "…")]`),
  `ParamMeta.doc`, and `MethodMeta::returns()` / `reference()`, the one
  renderer for the `help` tool and the generated guide.
- `#[api(no_output_schema)]` and `#[api(opaque)]`. In sense, `extract_doc`
  joins every line. In flux it returns the first paragraph plus the whole doc.
- In `McpApiServer` (mcp.rs, +236 lines): tiered tool lists with
  `list_changed` notifications, which is why flux's copy depends on
  `tokio/time`.
- `tests/protocol.rs`, which drives the server over rmcp's in-memory transport
  and reads the JSON-RPC frames it writes. That is why `futures` is a dev
  dependency.

Migrating sense onto this repo should therefore be a dependency swap plus
whatever its call sites need for the new `MethodMeta` fields, with nothing to
port back. Not done here.
