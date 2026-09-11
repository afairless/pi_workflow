# AGENTS.md — repository conventions

## Toolchain

- Rust `1.91.1` exactly — enforced by `rust-toolchain.toml`. Do not bump the
  channel without reviewing all pinned dependencies.
- All dependencies are pinned to exact versions in `Cargo.toml`. No `*` or
  loose constraints; disable unused default features.

## Quality gates (every commit)

```bash
cargo test                                    # all tests pass
cargo fmt --check                             # formatted
cargo clippy --all-targets --all-features -- -D warnings   # zero warnings
```

## Commit style

- Conventional commits (`feat:`, `fix:`, `chore:`, `docs:`, `test:`).
- One logical unit per commit; each commit leaves the suite green.
- Commit messages for worker rows come from the `TODO.md` plan table.

## Plan workflow

1. Research lives in `docs/research/`.
2. The implementation plan is `TODO.md` (commit-by-commit table).
3. Implement one step at a time: implement → `cargo test` → `cargo fmt
   --check` → `cargo clippy --all-targets --all-features -- -D warnings` →
   commit with the table's message → stop.

## Rust style rules

- No `unsafe`, no `unwrap()`/`expect()`/`panic!()` in application logic —
  use `Result`/`Option` and the `?` operator.
- `thiserror` for library errors, `anyhow` for the binary.
- Unit tests live in `#[cfg(test)] mod tests` next to the code; integration
  tests live in `tests/`.
