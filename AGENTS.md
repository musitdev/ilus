# AGENTS.md

Guidance for AI coding agents working on **ilus**.

## Project
- Language: Rust (stable, edition 2024)
- Domain: trading
- Specification: see [`SPECS.md`](SPECS.md) — the source of truth. Read it before implementing anything.

## Rules
- Do not implement features absent from `SPECS.md`. If something is unclear, ask or add it to *Open questions*.
- Keep changes small and focused; one feature per change.
- Use `rust_decimal::Decimal` for prices/quantities — never `f32`/`f64` for money.
- No `unwrap()`/`expect()` in non-test code; propagate errors with `Result`.
- Every behavior in an acceptance criterion must have a test.
- Never copy code, tests, or data from external projects (licensing). Implement from the spec only.

## Commands
```sh
cargo build
cargo test
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```
All four must pass before a change is considered done.

## Conventions
- Formatting: `rustfmt` defaults.
- Naming: standard Rust (`snake_case` fns/modules, `CamelCase` types).
- Docs: public items get `///` doc comments.
