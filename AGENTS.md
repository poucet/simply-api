# AGENTS — simply-kit

**Read @SYSTEM.md first.** It describes the working system: the roles, the doc
web and the landing rules, shared with every project run this way. This file
adds only what is specific to this repo.

`SYSTEM.md` and `.claude/agents` are gitignored symlinks into
`~/projects/simply/dev/`. They are missing from a fresh clone or a new
workspace. Recreate them with:

    mkdir -p .claude
    ln -sfn ~/projects/simply/dev/agents .claude/agents
    ln -sfn ~/projects/simply/dev/AGENTS.md SYSTEM.md

## What this repo is

**Generic infrastructure shared by more than one simply project.** The first
users are flux, tussen v2 and sense. "simply-kit" is a working name; see
README.md. A crate belongs here when two projects would otherwise each keep a
copy and drift apart. **Nothing here may depend on a project crate** (`flux-*`,
`tussen-*`, …): a kit that depends on a project drags the project into every
consumer. Musical vocabulary goes to `~/projects/simply/music-rs`.

Consumers pin a **git tag** on `https://github.com/poucet/simply-kit`, which is
public because tussen is open source. **Push a tag before any consumer
references it**, or that consumer's CI breaks. A change that a consumer needs is a commit here,
then a tag, then a bump to that tag in the consumer.
The version lives once, in `[workspace.package]`.

## Gates

Run both **unfiltered**. Report them verbatim with counts, and re-run them
after the final rebase:

    cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings

CI (`.github/workflows/ci.yml`) runs the same gates, plus `cargo fmt --check`.

## Docs

Decaying docs live in `docs/ops/` (`CHANGELOG.md`, `ISSUES.md`), and
investigations in `docs/research/`. Add the rest of the doc web only when a
document is needed.
