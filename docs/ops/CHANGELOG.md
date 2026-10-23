# Changelog

What got added, per landing — dated with a clock and the jj change id
(format: `~/projects/simply/dev/AGENTS.md` §2).

## 0.1.1 — 2026-10-23

#### MIT only (`LICENSE-MIT-ONLY`) · 2026-10-23 · kxrnwllw

- Licence is **MIT**, matching tussen; no longer `MIT OR Apache-2.0`.
  `LICENSE-MIT` is now `LICENSE`, `LICENSE-APACHE` is gone, every crate
  declares `license = "MIT"`. `v0.1.0` stays as published; depend on `v0.1.1`.

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
  repo yet. sense's own copy is an older flux one, with nothing flux lacks
  ([research](../research/sense-copy.md)).
