# simply-hooks

Signed outbound webhooks: an app emits an event, a background worker delivers it to each target with retries from a small persistent outbox.

An app calls [`Hooks::emit`]; a background worker delivers the event to every target that wants it, retrying from a small outbox kept on disk. The crate knows nothing about any receiver: it is the same scheme whoever listens.

```rust,no_run
use simply_hooks::{Config, Hooks, Target};
let hooks = Hooks::new(
    Config::new(Some("/path/to/app-data/hooks-outbox.json".into()), "my-app/1.0")
        .target(Target::new("https://example.com/hooks", "secret").events(["jam.*", "app.started"])),
)?;
hooks.emit("app.started", serde_json::json!({ "version": "1.0" })); // returns at once
# Ok::<(), std::io::Error>(())
```

## The wire scheme

Every delivery is one `POST` with a JSON body `{ "event": "<name>", "at": "<RFC 3339, UTC>", "data": { … } }` and these headers:

| Header | Value |
|---|---|
| `Content-Type` | `application/json` |
| `X-Hook-Delivery` | a UUID v4, the same on every attempt of one delivery |
| `X-Hook-Event` | the event's name, as in the body |
| `X-Signature` | signed targets only: `sha256=` + lowercase hex HMAC-SHA256(secret, `"<timestamp>.<raw body>"`) |
| `X-Timestamp` | signed targets only: when this attempt was signed, unix seconds |

A target has a secret or not ([`Target::unsigned`]): a public intake (a receiver that keeps no secret for the sender) takes unsigned deliveries, which carry no `X-Signature` and no `X-Timestamp`. A linked device ([`device`]) signs the same way with its own key, plus `X-Hook-Device: <device id>`.

A retry sends the same body and delivery id (signed: with a fresh timestamp and signature), so a receiver deduplicates on `X-Hook-Delivery` and refuses a timestamp far from its own clock.

**The test vector** (pin it in any implementation, sender or receiver):

* secret: `whsec_test_123`
* timestamp: `1700000000`
* body: `{"event":"jam.started","at":"2023-11-14T22:13:20Z","data":{"participants":2,"session":"3f2a9c"}}`
* `X-Signature`: `sha256=0f9a8cf6653bad9a5bcf9b884970aa7e37933e85c2316e33c681533688a003f4`

## Delivery

* A `2xx` answer delivers it. `408`, `425`, `429`, `5xx`, a timeout or a network error is retried with backoff ([`Retry`]), or after the answer's `Retry-After` seconds when that is later, until the delivery is older than [`Retry::max_age`]. Any other answer (a wrong secret's `401`, a `404`, a redirect) is refused for good.
* A body over [`Config::max_body`] (256 KiB by default, what Lumina takes) is refused at `emit`, logged, and never queued.
* `https://` only, but for a loopback host (`http://localhost:…`). Redirects are not followed and the answer's body is never read.
* Deliveries are independent and tried at once, unless emitted in a **sequence** ([`Hooks::emit_in`]): to each target, a delivery waits until the earlier ones in its sequence are delivered, across retries and restarts, and one given up on (refused, too old, or pushed out of a full outbox) takes the rest of its sequence with it. For an event that only makes sense after another (a `jam.ended` after its `jam.started`).
* The outbox ([`Config::outbox`]) holds every delivery not yet made, survives a restart, and loses a delivery once it is made, refused or too old. It never holds a secret.
* **Each event's own outcome**: `emit` hands back the event's [`EventId`], and [`Hooks::deliveries`] says what became of it at each target ([`DeliveryState`]: pending with its attempts and next try, delivered, or dropped and why).

## Consumers

Used by tussen (pinned by rev, behind its `hooks` feature) to send its events; Lumina is the receiver that verifies the scheme. Deliberately independent of `simply-api`, like `simply-relay`: any app can compile it in.

## Depending on it

Pin by rev (some crates, `simply-hooks` and `simply-relay`, are newer than the v0.1.1 tag):

```toml
[dependencies]
simply-hooks = { git = "https://github.com/simply-audio/simply-api", rev = "<commit>" }
```

Or by tag when the tag covers it:

```toml
[dependencies]
simply-hooks = { git = "https://github.com/simply-audio/simply-api", tag = "v0.1.1" }
```

## Licence

MIT — see [LICENSE](../../LICENSE).
