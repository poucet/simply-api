# simply-relay

An app dials out to Lumina over a WebSocket and serves its MCP tools there, gated by the member's consent.

The app opens one WebSocket to Lumina (`wss://…/devices/v1`), proves who it is in the first frame ([`wire`]), and then serves any rmcp 3.x [`ServerHandler`] over the socket, one JSON-RPC message per text frame ([`frames`]), with Lumina as the MCP client. Nothing on the member's side listens, so NAT and firewalls don't matter.

Between Lumina and the app's handler sits the [`gate`]: every tool is off until the member switches its class on, and a refused call never reaches the handler. Every call is written to the member's own call log ([`calls`]). The standard diagnostics ([`diag`]) are served beside the app's tools.

```rust,ignore
#[cfg(feature = "lumina")]
let relay = simply_relay::Relay::builder("tussen", env!("CARGO_PKG_VERSION"), config_dir)
    .read_only(["get_state", "levels", "mesh_peers"])
    .config(move || serde_json::to_value(hub.config()).unwrap_or_default())
    .redactor(|config| { config["studio"]["owner_email"] = "[redacted]".into(); })
    .spawn(move || McpApiServer::new(api.clone(), tussen_api_tool_router::<dyn TussenApi>()))?;
// once, where the app builds its tracing subscriber:
#[cfg(feature = "lumina")]
let subscriber = subscriber.with(simply_relay::log_layer());
```

The settings panel then calls [`Relay::pair`] with the code `/support link` showed, [`Relay::set_read_only`] / [`Relay::set_action`] for the switches, [`Relay::recent_calls`] for the log and [`Relay::disconnect`].

## Adopting it

An app compiles the relay in behind its own optional feature, so a build without it carries none of this:

```toml
[features]
lumina = ["dep:simply-relay"]
[dependencies]
simply-relay = { git = "https://github.com/simply-audio/simply-api", rev = "<commit>", optional = true }
```

Pin by rev: `simply-relay` and `simply-hooks` are newer than the v0.1.1 tag.

## Consumers

No app depends on it yet; Lumina is the server it dials. Deliberately independent of `simply-api`: it serves any rmcp 3.x `ServerHandler`, so an app can adopt it before it uses simply-api.

## Depending on it

Pin by rev (the crate is newer than the v0.1.1 tag):

```toml
[dependencies]
simply-relay = { git = "https://github.com/simply-audio/simply-api", rev = "<commit>" }
```

Or by tag when the tag covers it.

## Licence

MIT — see [LICENSE](../../LICENSE).
