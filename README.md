# simply-api

Shared infrastructure crates for the simply projects: one async trait that
derives MCP tools, dispatch and typed clients, plus the undo/redo history it
records operations into. Nothing in here knows about audio, music, or any one
app.

The repo is named after its main crate, `simply-api`. Three more crates are
what that crate is built on; `simply-relay` and `simply-hooks` stand on their own.

## Crates

| crate | what it is |
|---|---|
| `simply-api` | runtime for `#[api_service]`: `ApiResult`/`ApiError`, the `ServiceMeta` registry, and `McpApiServer`, a generic rmcp 3.x server over any `Arc<A>` |
| `simply-api-macros` | `#[api_service]`: from one async trait, generates params structs, the MCP tool router, a `Caller` dispatcher, a `Remote{Trait}` client and a `Dyn{Trait}` alias |
| `simply-history` | a generic undo/redo history tree: arena, MRU branches, undo/redo/goto, prune, serde, a journal |
| `simply-op-macros` | `#[derive(Op)]`: read-vs-mutate classification and label templates for operation enums |
| `simply-relay` | an app dials out to Lumina over a WebSocket and serves any rmcp 3.x `ServerHandler` there: pairing by code, a token file rotated on each connect, the member's consent switches enforced in the app, a local call log, and standard read-only diagnostics. It does not depend on `simply-api`. Apps compile it in behind their own optional `lumina` feature |
| `simply-hooks` | signed outbound webhooks: `emit(event, data)` returns at once, a worker thread POSTs `{event, at, data}` to each target that wants it, signed `X-Signature: sha256=HMAC(secret, "<X-Timestamp>.<body>")` with `X-Hook-Delivery` and `X-Hook-Event` (the scheme Lumina verifies), with retries from a small outbox file that survives a restart. The scheme and its test vector are in the crate docs. It does not depend on `simply-api`, and adds no crate this workspace didn't already build |
| `relay-compat` | dev only, never published: tests that Lumina's rmcp 1.3 client and the apps' rmcp 3.x servers talk over the relay |

A consumer depends on `simply-api` alone and gets the other three through it,
or on `simply-history` + `simply-op-macros` alone for an undo tree with no API.

External dependencies (versions as resolved in flux's `Cargo.lock` at
`bae34fcf`; this repo's lockfile was seeded from it): `rmcp 3.1.4` (server),
`schemars 1.2.2`, `serde 1.0.229`, `serde_json 1.0.151`, `async-trait 0.1.92`,
`tokio 1.53.1` (`time`), and `syn 2` / `quote 1` / `proc-macro2 1` for the
two proc-macro crates. Dev only: `futures 0.3`, tokio's runtime features.

## Using it

Depend on a tag, declared once at the consumer's workspace root:

```toml
[workspace.dependencies]
simply-api = { git = "https://github.com/simply-audio/simply-api", tag = "v0.1.0" }
# or, for an undo tree with no API:
simply-history = { git = "https://github.com/simply-audio/simply-api", tag = "v0.1.0" }
simply-op-macros = { git = "https://github.com/simply-audio/simply-api", tag = "v0.1.0" }
```

`Cargo.lock` pins the commit. A newer tag means bumping `tag` and running
`cargo update -p simply-api`. The crates are not on crates.io.

A minimal service:

```rust,ignore
use async_trait::async_trait;
use simply_api::{api_service, ApiResult};

#[api_service]
#[async_trait]
pub trait CounterApi: Send + Sync {
    /// Add `by` to the counter and return the new value.
    async fn add(
        &self,
        /// How much to add.
        by: i64,
    ) -> ApiResult<i64>;
}
```

This generates `CounterRequest`/`CounterResponse`, a `CounterApiDispatcher`
(`Caller`), a `RemoteCounterApi` client and `counter_api_tool_router::<A>()`.
Pass the router to `simply_api::mcp::McpApiServer::new(api, router)` and serve
it over any rmcp transport. The consumer also depends on `async-trait`,
`serde` and `schemars` for its own types.
`crates/simply-api/tests/derive.rs` is the worked example.

## Licence

MIT — see [LICENSE](LICENSE).

## Provenance

The four crates were **extracted from flux at `bae34fcf`** (change
`okwotpou`), `crates/simply-{api,api-macros,history,op-macros}`. The code is
byte for byte the same, except for:

- each `Cargo.toml` inherits from this workspace as it did from flux's, and
  adds `repository`, `authors` and `readme`. The version is reset to `0.1.0`
  (flux's was `0.7.5`);
- doc comments that pointed at flux-only files were reworded: a research note
  that had left three sentences dangling (`… —.`, `See.`), and the old path of
  flux's history adapter;
- `rust-toolchain.toml` (1.95.0) and `rustfmt.toml` are flux's.

flux, tussen and sense are not yet migrated onto this repo. Each still carries
its own copy, or none.

## Development

    cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all --check

CI (`.github/workflows/ci.yml`) runs all three on Linux and macOS.
