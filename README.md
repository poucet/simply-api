# simply-kit

Shared infrastructure crates for the simply projects: one async trait that
derives MCP tools, dispatch and typed clients, plus the undo/redo history it
records operations into. Nothing in here knows about audio, music, or any one
app.

"simply-kit" is a working name. It is spelled in this README, `AGENTS.md`, and
the top comment of `Cargo.toml`; a rename touches those and the directory.

## Crates

| crate | what it is |
|---|---|
| `simply-api` | runtime for `#[api_service]`: `ApiResult`/`ApiError`, the `ServiceMeta` registry, and `McpApiServer`, a generic rmcp 3.x server over any `Arc<A>` |
| `simply-api-macros` | `#[api_service]`: from one async trait, generates params structs, the MCP tool router, a `Caller` dispatcher, a `Remote{Trait}` client and a `Dyn{Trait}` alias |
| `simply-history` | a generic undo/redo history tree: arena, MRU branches, undo/redo/goto, prune, serde, a journal |
| `simply-op-macros` | `#[derive(Op)]`: read-vs-mutate classification and label templates for operation enums |

A consumer depends on `simply-api` alone and gets the other three through it,
or on `simply-history` + `simply-op-macros` alone for an undo tree with no API.

External dependencies (versions as resolved in flux's `Cargo.lock` at
`bae34fcf`; this repo's lockfile was seeded from it): `rmcp 3.1.4` (server),
`schemars 1.2.2`, `serde 1.0.229`, `serde_json 1.0.151`, `async-trait 0.1.92`,
`tokio 1.53.1` (`time`), and `syn 2` / `quote 1` / `proc-macro2 1` for the
two proc-macro crates. Dev only: `futures 0.3`, tokio's runtime features.

Licence: `MIT OR Apache-2.0`, the same as flux, and compatible with tussen's MIT.

## Using it

By git tag, from the workspace root of the consumer:

    simply-api = { git = "<url>", tag = "v0.1.0" }

There is no remote yet. Until there is, a local consumer can point at
this checkout with `git = "file:///Users/simplychris/projects/simply/simply-kit"`.
The repo is colocated, so cargo can read the tag.

## Provenance

The four crates were **extracted from flux at `bae34fcf`** (change
`okwotpou`), `crates/simply-{api,api-macros,history,op-macros}`, byte for byte except for:

- each `Cargo.toml` inherits from this workspace exactly as it did from flux's;
  only the version is reset to `0.1.0` (flux's was `0.7.5`);
- three doc comments that ended in a dangling pointer to a flux research note
  (`… —.`, `See.`) were closed off;
- `rust-toolchain.toml` (1.95.0) and `rustfmt.toml` are flux's.

flux, tussen and sense are not yet migrated onto this repo. Each still carries
its own copy, or none.

## The divergent copy in sense

`~/projects/simply/sense` (main `9644bfff`) carries its own copy of all four
crates, snapshotted from flux on 2026-09-03/04. The comparison is in
[`docs/research/sense-copy.md`](docs/research/sense-copy.md). In short, **sense
has nothing flux lacks.** Its copy is an older flux. Every difference is
either something flux added after the snapshot or a formatting difference.

## Gates

    cargo test --workspace
    cargo clippy --workspace --all-targets     # 0 warnings
