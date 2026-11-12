# simply-op-macros

`#[derive(Op)]` — operation metadata from `#[op(...)]` annotations.

On an operation **enum** (one variant per operation — e.g. an api's generated request enum) or a single operation **struct**, generates an `impl simply_history::OpMeta`:

```rust,ignore
#[derive(Op)]
enum Request {
    /// Reads never enter history or a journal.
    #[op(read)] // the default — may be omitted
    GetGraph {},

    /// Mutations do; the label template interpolates the fields.
    #[op(mutates, label = "connect {from}->{to}")]
    Connect { from: String, to: String },
}
```

* `mutates` / `read` — the classification `OpMeta::mutates` reports; history/journal machinery brackets transactions only around mutations.
* `label = "..."` — an inline-capture `format!` template over the operation's own (named) fields; without one, the label is the operation's snake_case name.
* `#[op(crate = "path")]` on the container re-points the generated `simply_history` path (for re-exports), serde-style.

## Consumers

Used by flux, sense and tussen through `simply-api`, which re-exports it. It annotates the request enums `#[api_service]` generates. You depend on `simply-api`, not on this crate directly.

## Depending on it

Through `simply-api`:

```toml
[workspace.dependencies]
simply-api = { git = "https://github.com/simply-audio/simply-api", tag = "v0.1.1" }
```

Or on its own with `simply-history`:

```toml
[workspace.dependencies]
simply-history = { git = "https://github.com/simply-audio/simply-api", tag = "v0.1.1" }
simply-op-macros = { git = "https://github.com/simply-audio/simply-api", tag = "v0.1.1" }
```

## Licence

MIT — see [LICENSE](../../LICENSE).
