# simply-api-macros

`#[api_service]` — one async trait, every surface derived.

Ported from lumina's `simply-rpc` `#[rpc_service]` (server dispatch + typed client from one trait) and its daemon `#[skill_router]` (rmcp tool glue), re-targeted at rmcp 3.x: the MCP tool router is generated directly from the trait, so a new capability is one trait method — never a hand-written tool wrapper.

```rust,ignore
#[simply_api::api_service]
#[async_trait]
pub trait FluxApi: Send + Sync {
    /// Tool description comes from the doc comment.
    async fn add_node(
        &self,
        /// Field descriptions come from parameter doc comments.
        kind: String,
        x: Option<f32>,
    ) -> ApiResult<NodeAdded>;

    #[api(no_tool)]
    async fn internal_only(&self) -> ApiResult<()>;

    #[api(name = "render_wav")]
    async fn render(&self, seconds: f32) -> ApiResult<RenderResult>;
}
```

Generated alongside the (unchanged) trait: per-method params structs, a `ServiceMeta` registry, an rmcp `ToolRouter` factory, a `Caller` dispatcher, a `Remote{Trait}` client, and a `Dyn{Trait}` alias. Place `#[api_service]` **above** `#[async_trait]`.

## Consumers

Used by flux, sense and tussen through `simply-api`, which re-exports it. You depend on `simply-api`, not on this crate directly.

## Depending on it

```toml
[dependencies]
simply-api = { git = "https://github.com/simply-audio/simply-api", tag = "v0.1.1" }
```

## Licence

MIT — see [LICENSE](../../LICENSE).
