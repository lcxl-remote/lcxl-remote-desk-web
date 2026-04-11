# Repository Guidelines

## Project Structure & Module Organization
This repository is a Rust workspace with a Vite frontend.

- `server/`: main desktop service (Actix-Web, WebRTC, REST/OpenAPI) with integration tests in `server/tests/`.
- `signal/`, `turn/`, `signal-facade/`, `server-user/`, `server-version/`, `utils/`: supporting Rust crates.
- `vite-project/`: React + TypeScript UI (`src/features`, `src/components`, `src/services`).
- `tauri-app/src-tauri/`: desktop shell for privacy-screen and whiteboard features.
- `conf/config.toml`: runtime config; `openapi.json` + `vite-project/openapi.json`: API specs.

## Build, Test, and Development Commands
Run commands from repo root unless noted.

- `cargo run -p lcxl-remote-desk-server`: start backend in default mode.
- `cargo run -p lcxl-remote-desk-server -- --help`: view startup flags (`-m default|signaling|desk-server`).
- `cargo build --workspace --release`: build all Rust crates.
- `cargo test --workspace`: run Rust tests (including `server/tests/test_utils.rs`).
- `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings`: format and lint backend.
- `cd vite-project && npm ci && npm run dev`: start frontend at Vite dev server (default `5174`).
- `cd vite-project && npm run build`: type-check and build frontend.
- `cd vite-project && ./update_openapi.ps1`: refresh frontend API client from `http://localhost:8081/openapi.json`.

## Coding Style & Naming Conventions
- Rust: follow `rustfmt`, snake_case for modules/files/functions, PascalCase for types, `SCREAMING_SNAKE_CASE` for constants.
- TypeScript/React: 4-space indentation in current code, PascalCase for components, `useXxx` for hooks, kebab-case filenames in `src/components/ui`.
- Keep generated API artifacts under `vite-project/src/services/`; do not hand-edit generated hook/type files.

## Testing Guidelines
- Prefer crate-local unit tests and integration tests under `server/tests/`.
- Test names should describe behavior (example: `test_rejects_invalid_turn_secret`).
- For frontend changes, at minimum validate with `npm run build` and manual flow checks (see `vite-project/test_flow.mjs` when relevant).

## Commit & Pull Request Guidelines
- Follow Conventional Commits (`feat:`, `fix:`, `chore:`), consistent with recent history.
- Keep commits focused and functional; include config/schema updates in the same commit when required.
- PRs should include: concise description, affected modules (e.g., `server/service/signaling`), test/verification steps, linked issues, and UI screenshots for frontend changes.

## Security & Configuration Tips
- Never commit real credentials; use `conf/config.toml` placeholders for local dev.
- Review changes touching auth, signaling, TURN, or file-transfer paths with extra care.
