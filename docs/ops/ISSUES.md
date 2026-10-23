# Issues

A parking lot, not a queue (`~/projects/simply/dev/AGENTS.md` §2).

| status | date | id | issue |
|---|---|---|---|
| 🔴 | 2026-10-23 | I-001 | `cargo doc` warns three times: intra-doc links to private items (`codegen` in simply-api-macros's crate doc, and `SUPPORTED_PROTOCOL_VERSIONS` twice in simply-api's `supported_protocol_versions`). These were inherited from flux. `cargo doc` is not a gate. |
| 🔴 | 2026-10-23 | I-002 | No remote, so consumers cannot depend on `tag = "v0.1.0"` by URL. Until a remote exists, the only option is a `file://` git URL to this checkout, which works on this machine only and not in CI. |
