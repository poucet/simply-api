# Changelog

What got added, per landing — dated with a clock and the jj change id
(one entry per landing, newest first).

## Unreleased

#### `simply-hooks`: linked devices · 2026-11-10 09:49 · nnnksosu

- An app install can be **linked** to its member's account at a receiver
  (Lumina's `/<app> link`): `device::link(base, request, timeout)` swaps
  the one-time code the member was shown (read in any case, dashes
  dropped) for a device id and a key, at `POST /devices/v1/link`;
  `device::unlink` tells the receiver to forget it, signed by the device.
  A refusal says its status, the receiver's reason and its `Retry-After`.
- `Target::device(url, device_id, key)` signs deliveries with the device's
  key and adds `X-Hook-Device: <device id>` to every attempt, retries from
  the outbox included. Any target may carry headers of its own
  (`Target::header`); one with a line break is never sent.
- The `keychain` feature keeps a device's key in the macOS keychain
  (`keys::Keychain`); off macOS it keeps nothing and says so.
  `keys::MemoryKeys` is for tests.
- A delivery refused for good now says the receiver's reason in the
  status (`refused: HTTP 401 (device not linked)`), not only its code.
- The crate docs pin Lumina's device-signing test vector.

#### `simply-hooks`: signed outbound webhooks · 2026-11-09 10:15 · wnxonwuo, yzloxkmn, sqlzlqtw, lzmurqlqktsn

- New crate **`simply-hooks`**. An app calls `emit(event, data)` and it
  returns at once; a worker thread POSTs
  `{"event", "at", "data"}` to every target that wants the event. Each
  target has a URL, an optional secret and an optional event list
  (`jam.*` style prefixes allowed). A target without a secret
  (`Target::unsigned`, for a public intake) gets no `X-Signature` and no
  `X-Timestamp`. The app needs no async runtime.
- Every request carries the headers Lumina's hooks listener verifies and
  simply.audio sends: `X-Signature: sha256=<hex HMAC-SHA256(secret,
  "<timestamp>.<body>")>`, `X-Timestamp` (unix seconds), `X-Hook-Delivery`
  (a UUID v4) and `X-Hook-Event`. A retry keeps the delivery id and body
  and signs again. The crate docs hold a test vector other repos can pin,
  and `sign::verify` for receivers (several comma-separated signatures
  pass if one does).
- Retries: 10 s doubling to 10 min, for a day; a 10 s timeout. A `2xx`
  delivers; `408`, `425`, `429`, `5xx` and network errors retry; any other
  answer is refused for good. A `Retry-After` longer than the backoff is
  waited out (up to an hour). A body over 256 KiB, Lumina's limit, is
  refused at `emit`. `https://` only, except to localhost.
- Pending deliveries are kept in one owner-only JSON file in the app's data
  folder, so they survive a restart; it never holds a secret. Failures are
  logged with the event, the delivery id and the host: never the body.
- `status()` gives a settings screen what is pending, delivered and
  dropped, and the last error, per target. `set_targets` applies changed
  settings while the app runs.
- Nothing is added to the lockfile: the HTTP client is one POST over
  tokio and rustls, which the workspace already built.
- The receiver's whole answer (headers and body) is read, so a later
  follow-up to a bug report can pick up a session id from the reply and
  long-poll with the same signed POST. Nothing changes for apps yet.

#### `simply-relay`: an app can link itself to Lumina · 2026-11-09 08:35 · vmlmllrk..ouppwpny

- New crate **`simply-relay`**. An app (tussen, flux) opens a WebSocket out to
  Lumina and serves its existing MCP tools there, so Lumina can check a
  member's setup when they report a bug. Nothing on the member's side
  listens, so NAT and firewalls don't matter. The crate does not depend on
  `simply-api`, so an app can adopt it before it migrates to `simply-api`.
- The member links an app by pasting the one-time code `/support link`
  shows. The app keeps one file, `lumina.toml`, readable by the member
  only (0600). It holds the device token, which Lumina replaces on every
  connect, and the consent switches.
- Everything is **off by default**. One switch allows read-only diagnostics.
  Each action class (jam sessions, later) has its own switch. A tool that
  isn't switched on is neither listed nor run: the app refuses it before its
  own code sees it. Each app lists its read-only tools by hand.
- Every call Lumina makes, allowed or refused, is written to
  `lumina-calls.log` beside the token file, with the reason Lumina gave.
  Argument values are not written.
- Every app gets the same diagnostics: `lumina_about`, `lumina_config`
  (secrets blanked by the app's redactor and then by key name, home
  directory shown as `~`), `lumina_errors`, `lumina_log` and
  `lumina_report`. A tracing layer, `simply_relay::log_layer()`, keeps the
  log lines they read.
- Apps compile it in behind their own optional `lumina` feature.
- A test shows Lumina's rmcp 1.3 client working with the apps' rmcp 3.x
  servers over this connection, protocol `2025-06-18`. It lives in the
  dev-only crate `relay-compat`, which is never published.

## 0.1.1 — 2026-10-23

#### MIT only (`LICENSE-MIT-ONLY`) · 2026-10-23 · kxrnwllw

- Licence is **MIT**, matching tussen; no longer `MIT OR Apache-2.0`.
  `LICENSE-MIT` is now `LICENSE`, `LICENSE-APACHE` is gone, every crate
  declares `license = "MIT"`. `v0.1.0` stays as published; depend on `v0.1.1`.
- Public-repo cleanup: no local machine paths in the docs, the internal
  sense comparison note removed, `cargo doc` warning-free (I-001).

## 0.1.0 — 2026-10-23

#### Published as `poucet/simply-api` (`API-PUBLISH`) · 2026-10-23 13:58 · ywtzoswx

- The repo is named `simply-api`, after its main crate. It was `simply-kit`
  while the name was being decided. It is public at
  https://github.com/poucet/simply-api, with `main` and `v0.1.0` pushed.
- Consumers depend on it like this:
  `simply-api = { git = "https://github.com/poucet/simply-api", tag = "v0.1.0" }`.

#### Ready to publish: CI, licence files, Cargo metadata (`KIT-PUBLISH-PREP`) · 2026-10-23 13:57 · rqqsmoty

- CI runs `cargo fmt --check` on Linux, and `cargo test` and `cargo clippy -D
  warnings` (both `--locked`) on Linux and macOS. The workflow is modelled on
  flux's `ci.yml`.
- `LICENSE-MIT` and `LICENSE-APACHE` are added. Every crate now declares
  `repository`, `authors` and `readme`.
- The README has a "Using it" section (a git dependency by tag, plus a minimal
  service checked to compile) and the dual-licence statement.
- Crate docs no longer name a flux-only file path.

#### The API derive and the history tree get their own repo (`KIT-EXTRACT`) · 2026-10-23 13:53 · orlnwpsz

- `simply-api`, `simply-api-macros`, `simply-history` and `simply-op-macros`
  are extracted from flux at `bae34fcf` with the same code and the same
  dependency versions. The versions are reset to 0.1.0. Licence:
  `MIT OR Apache-2.0`.
- Nothing changes yet for flux, tussen or sense: none of them depends on this
  repo yet.
