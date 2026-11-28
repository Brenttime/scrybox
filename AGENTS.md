# AGENTS.md

## Fork policy

Scrybox (`Brenttime/scrybox`) is a hard fork of Bindarr (`thenotoriousJeremy/bindarr`).

- **Never open pull requests, issues, or pushes against the upstream source repo** (`thenotoriousJeremy/bindarr`, the `upstream` remote).
- All PRs target `Brenttime/scrybox` (`origin`) only. When using `gh pr create`, always pass `--repo Brenttime/scrybox` so it does not default to the fork parent.
- The `upstream` remote is fetch-only for reference; do not push to it.

## Current architecture

- Production is one Rust container built with `docker build -f Dockerfile.rust` (server crates in `server/crates/{scrybox-server,local-scryfall}`). The local Scryfall engine (portal at `/scryfall-portal/`) and cardscan (ONNX) run in-process. There is no Node backend; `backend/` was removed in PR #170. Node/npm is only used for `frontend/`.
- Compatibility identifiers keep the old name: `bindarr.db`, `BINDARR_*` env, container `bindarr`, `bindarr-data` volume, `bindarr_*` localStorage keys. Use **Scrybox** in prose.

## Rust standards

Every Rust diff must meet this bar (from PR #169):

- No `unwrap`/`expect`/`panic!` on production paths that can fail (input, IO, DB, network, image data). Propagate to the existing error type or handler error response. Tests may unwrap.
- A genuinely infallible call uses `.expect("why")` plus a `// INFALLIBLE: <why>` comment.
- Every `unsafe` block carries a `// SAFETY:` comment.
- Silence lints with `#[expect(lint, reason = "...")]`, not `#[allow]`.
- Build and run Rust tests in Docker (`Dockerfile.rust`, `rust-build` stage); the host lacks libclang.
- Do not run `cargo fmt --all`: the tree is not rustfmt-clean and it rewrites unrelated files. Check edited files with `rustfmt --edition 2021 --emit stdout <file> >/dev/null`.
