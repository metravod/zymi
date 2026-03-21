# Contributing to Zymi

## Getting started

```bash
git clone https://github.com/metravod/zymi
cd zymi
cargo build
cargo test
```

## Before submitting a PR

1. **Tests pass**: `cargo test`
2. **No clippy warnings**: `cargo clippy -- -D warnings`
3. **Builds clean**: `cargo build`

## Code style

- Follow existing patterns in the codebase
- No `unsafe` code
- Prefer `Result` over `unwrap()`/`expect()` in library code (fine in tests)
- Keep functions focused — if it's doing too much, split it

## Pull requests

- One feature or fix per PR
- Write a clear description of what changed and why
- Add tests for new functionality
- Update README.md if you add user-facing features

## Reporting issues

Use GitHub Issues. Include:
- What you expected vs what happened
- Steps to reproduce
- OS, Rust version (`rustc --version`), zymi version (`zymi --version`)

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
