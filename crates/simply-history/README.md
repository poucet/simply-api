# simply-history

A generic undo/redo history **tree** over an op log — no domain deps.

Extracted from Simply Flux's engine history; designed for any document-shaped state whose edits record reversible primitive ops:

* **Ops** ([`Op`]): the primitive reversible mutations a transaction recorded. Consumers whose op type is local implement [`Op`] (one `invert`); the [`State`] seam also lets a consumer whose op type lives in another crate (orphan rule) supply the inversion there instead.
* **Transactions** ([`Transaction`]): the history atom — a labeled list of ops plus whatever metadata the consumer carries (author, scope, provenance, …). The tree stores the consumer's own record type, so its serialized shape is entirely the consumer's.
* **State** ([`State`]): where ops (re)apply, plus the resource-reclaim hook — called exactly once per op of a transaction leaving the tree forever (pruned), the single place e.g. id reservations end.

One shared tree per document: undo walks to the parent by replaying the ops inverted in reverse; redo follows the *most recently used* child (vim-style); a new edit after undo **branches** instead of discarding the redone future. [`History::goto`] jumps anywhere by replaying along the path through the common ancestor. Pruning is size-capped and only ever removes abandoned leaves — never the path from the root to the current position.

**Persistence**: the tree serializes ([`HistoryRepr`]) as a positional arena image — index *is* the [`HistoryId`], pruned slots stay `null` — so ids survive save/load verbatim. The envelope carries a major [`FORMAT_VERSION`] (breaking changes only) and an additive [`FORMAT_MINOR_VERSION`] (defaulted fields, new metadata — readers accept any minor).

## Consumers

Used by flux (the engine's undo tree, extracted from there) and sense (the timeline's undo tree). Both implement [`State`] and call `undo`, `redo`, `goto` and `prune`. A consumer can depend on `simply-history` + `simply-op-macros` alone for an undo tree with no API.

## Depending on it

Through `simply-api`:

```toml
[workspace.dependencies]
simply-api = { git = "https://github.com/simply-audio/simply-api", tag = "v0.1.1" }
```

Or on its own:

```toml
[workspace.dependencies]
simply-history = { git = "https://github.com/simply-audio/simply-api", tag = "v0.1.1" }
simply-op-macros = { git = "https://github.com/simply-audio/simply-api", tag = "v0.1.1" }
```

## Licence

MIT — see [LICENSE](../../LICENSE).
