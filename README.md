# simply-api

Shared infrastructure crates for the simply projects: one async trait that
derives MCP tools, dispatch and typed clients, the undo/redo history it
records operations into, and headless egui rendering to PNG. Nothing in here
knows about audio, music, or any one app.

The repo is named after its main crate, `simply-api`. Three more crates are
what that crate is built on; `simply-offscreen` stands alone.

## Crates

| crate | what it is |
|---|---|
| `simply-api` | runtime for `#[api_service]`: `ApiResult`/`ApiError`, the `ServiceMeta` registry, and `McpApiServer`, a generic rmcp 3.x server over any `Arc<A>` |
| `simply-api-macros` | `#[api_service]`: from one async trait, generates params structs, the MCP tool router, a `Caller` dispatcher, a `Remote{Trait}` client and a `Dyn{Trait}` alias |
| `simply-history` | a generic undo/redo history tree: arena, MRU branches, undo/redo/goto, prune, serde, a journal |
| `simply-op-macros` | `#[derive(Op)]`: read-vs-mutate classification and label templates for operation enums |
| `simply-offscreen` | headless egui rendering: run a frame in an `egui_kittest` wgpu harness until it settles and return it as PNG bytes, for snapshot tests and screenshot tools |

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
simply-api = { git = "https://github.com/poucet/simply-api", tag = "v0.2.0" }
# or, for an undo tree with no API:
simply-history = { git = "https://github.com/poucet/simply-api", tag = "v0.2.0" }
simply-op-macros = { git = "https://github.com/poucet/simply-api", tag = "v0.2.0" }
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

### Offscreen rendering

```toml
simply-offscreen = { git = "https://github.com/poucet/simply-api", tag = "v0.2.0" }
```

```rust,ignore
// One settled frame, egui's default style, 200×100 pt at 2× → 400×200 px.
let png = simply_offscreen::render_png(|ui| { ui.label("hello"); }, 200.0, 100.0, 2.0)?;
std::fs::write("hello.png", &png.bytes)?;

// The general form: `setup` themes the context once before the first frame
// (fonts, visuals); `pose` drives the harness after it settles (a drag, a
// key held) and the capture is the frame the gesture is in.
let png = simply_offscreen::render_png_with(apply_theme, draw, 200.0, 100.0, 2.0, |harness| {
    /* harness.event(…); harness.step(); */
})?;
```

`render_png_posed` is `render_png_with` with no setup. The harness steps at
60 Hz, runs `SETTLE_FRAMES` (3) frames after the setup pass, uses hardware
texture filtering, and spells keys for the OS it runs on.

It needs a wgpu backend at runtime (Metal, Vulkan or GL). Without one, every
call returns an `Err` starting with `simply_offscreen::UNAVAILABLE` rather
than panicking; the crate's render tests skip on it, so they pass on a CI
runner with no GPU. egui 0.36 needs rustc 1.95 (pinned in
`rust-toolchain.toml`). Versions as in flux at `bae34fcf`: `egui 0.36.1`,
`egui_kittest 0.36.1` (`wgpu`), `egui-wgpu 0.36.1`, `image 0.25.10` (`png`);
the lockfile entries were seeded from flux's.

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

`simply-offscreen` (0.2.0) is the **generic part of flux-editor's
`shell/offscreen.rs` at flux `bae34fcf`** (change `okwotpou`): the same
harness setup, settle loop, PNG encoding and `catch_unwind` wrapper.
`render_editor_png` and flux's theme are left out; the theme's place is the
`setup` argument of `render_png_with`. flux keeps its own copy for now.

flux, tussen and sense are not yet migrated onto this repo. Each still carries
its own copy, or none.

## Development

    cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all --check

CI (`.github/workflows/ci.yml`) runs all three on Linux and macOS.
