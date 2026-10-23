# Issues

A parking lot, not a queue (`~/projects/simply/dev/AGENTS.md` §2).

| status | date | id | issue |
|---|---|---|---|
| 🔴 | 2026-10-23 | I-001 | `cargo doc` warns three times: intra-doc links to private items (`codegen` in simply-api-macros's crate doc, and `SUPPORTED_PROTOCOL_VERSIONS` twice in simply-api's `supported_protocol_versions`). These were inherited from flux. `cargo doc` is not a gate. |
| 🔴 | 2026-10-23 | I-002 | Not published: there is no GitHub repo and no remote. flux and tussen must not switch to `git = "https://github.com/poucet/simply-kit"` until it is public, or their CI breaks. The only interim option is a `file://` URL to this checkout, which works on this machine and not in CI. Waiting on Chris to confirm the name. |
