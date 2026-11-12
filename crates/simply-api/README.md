# simply-api

One async trait → every surface derived.

`#[api_service]` on an `#[async_trait]` trait derives, with zero marginal work per capability:

* an **MCP tool router** (rmcp 3.x): each method is a tool — name from the method (or `#[api(name = "...")]`), description from its doc comment, input schema from a generated per-method params struct (schemars; parameter doc comments become field descriptions), output schema from the return type; `#[api(no_tool)]` opts a method out;
* a **dispatcher** (`{Trait}Dispatcher`) and a **typed remote client** (`Remote{Trait}` over any [`Caller`]) — the seam a real remote transport plugs into; in-process callers just use `Arc<dyn Trait>` (`Dyn{Trait}`) directly;
* a **method registry** (`{TRAIT}_META`) for lint tests and doc generation.

The pattern is ported from lumina's `simply-rpc` (`#[rpc_service]`) and its daemon `#[skill_router]`; HTTP/REST route generation is deliberately omitted for now.

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

This generates `CounterRequest`/`CounterResponse`, a `CounterApiDispatcher` (`Caller`), a `RemoteCounterApi` client and `counter_api_tool_router::<A>()`. Pass the router to `simply_api::mcp::McpApiServer::new(api, router)` and serve it over any rmcp transport. The consumer also depends on `async-trait`, `serde` and `schemars` for its own types. `tests/derive.rs` is the worked example.

## What it provides

* Runtime for `#[api_service]`: `ApiResult<T>` / `ApiError`, the `ServiceMeta` registry, and `McpApiServer`, a generic rmcp 3.x server over any `Arc<A>`.
* `simply_api::export::*` — re-exports of `async_trait`, `rmcp`, `schemars`, `serde`, `serde_json`, `simply_history` and `simply_op_macros`, so generated code needs no direct dependency on (or version agreement with) them.

## Consumers

Used by flux, sense and (soon) tussen. All three define an `#[api_service]` trait and serve it over MCP. A consumer depends on `simply-api` alone and gets `simply-api-macros`, `simply-history` and `simply-op-macros` through it, or on `simply-history` + `simply-op-macros` alone for an undo tree with no API.

## Depending on it

Depend on a tag, declared once at the consumer's workspace root:

```toml
[workspace.dependencies]
simply-api = { git = "https://github.com/simply-audio/simply-api", tag = "v0.1.1" }
```

`Cargo.lock` pins the commit. A newer tag means bumping `tag` and running `cargo update -p simply-api`. The crates are not on crates.io.

## Licence

MIT — see [LICENSE](../../LICENSE).
