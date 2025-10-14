# Twitch Overlay Monorepo Overview

This repository is implemented in lockstep with the product documentation inside the `.docs/` directory. Start with `.docs/00-index.md` to find the relevant specifications for each subsystem.

## Layout

- `crates/` — Rust workspace members
  - `app` — HTTP entrypoint (`/healthz` in PR-0)
  - `core` — Domain layer (stubs today)
  - `storage` — Persistence layer (stubs today)
  - `twitch` — Twitch API integration (stubs today)
  - `util` — Shared helpers (`APP_BIND_ADDR` parsing, dotenv loader)
- `web/overlay` — React + TypeScript overlay scaffold powered by Vite
- `.github/workflows/ci.yml` — Rust + frontend CI pipeline
- `scripts/` — Local development helpers for Linux (`dev.sh`) and Windows (`dev.ps1`)

Refer to `.docs/08-implementation-plan.md` for the milestone-oriented roadmap.

## Verification snapshot (PR-0)

The current branch was validated against the PR-0 Definition of Done with the
following commands:

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `npm ci`
- `npm run lint`
- `npm run typecheck`
- `npm run build`
- `APP_BIND_ADDR=127.0.0.1:18080 cargo run -p twi-overlay-app`
- `curl -i http://127.0.0.1:18080/healthz`

All commands completed successfully, confirming the scaffolded workspace builds
cleanly and the `/healthz` endpoint responds with HTTP 200.
