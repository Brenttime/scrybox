# AGENTS.md

## Fork policy

Scrybox (`Brenttime/scrybox`) is a hard fork of Bindarr (`thenotoriousJeremy/bindarr`).

- **Never open pull requests, issues, or pushes against the upstream source repo** (`thenotoriousJeremy/bindarr`, the `upstream` remote).
- All PRs target `Brenttime/scrybox` (`origin`) only. When using `gh pr create`, always pass `--repo Brenttime/scrybox` so it does not default to the fork parent.
- The `upstream` remote is fetch-only for reference; do not push to it.
