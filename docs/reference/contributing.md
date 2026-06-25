# Contributing

Contributions are welcome! This page summarizes the workflow and coding standards. See the repository's `CONTRIBUTING.md` for the authoritative version.

## Development Workflow

1. Set up the toolchain — Rust 1.90+ and Node.js 20+ (see [Quick Start](/guide/quick-start) and [Deployment](/guide/deployment) for system dependencies).
2. Run the backend with `cargo run` and the frontend with `npm run dev` in `vite-project/`.
3. Add tests for your change — **every code change must add test cases.**
4. Format and lint before submitting.

## Coding Standards

### Rust

- Format with `rustfmt`; run `cargo clippy`.
- `snake_case` for functions/modules, `PascalCase` for types, `SCREAMING_SNAKE_CASE` for constants.
- **Comments must be in English** and describe the code's *current* behavior — no development-phase markers.

### TypeScript / React

- 4-space indentation; `PascalCase` components; `useXxx` hooks; `kebab-case` filenames under `components/ui`.
- **Internationalization is mandatory** — all user-visible text must go through `t()`; add every new key to both `zh-CN` and `en-US` locale files. No hardcoded strings.
- Generated files under `src/services/` (Kubb output) must not be edited by hand.

## Commit Messages

- Use English and follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `chore:`, …).

## Building

```bash
# Backend
cargo build --release

# Frontend
cd vite-project && npm run build
```

See the [Module Map](/reference/modules) for where things live and the step-by-step recipes for adding APIs and signaling types.
