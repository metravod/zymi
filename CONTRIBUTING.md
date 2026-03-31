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

## Module structure

```
src/
├── core/           # Agent loop, LLM providers, config
├── connectors/     # CLI TUI, Telegram bot
├── events/         # Event bus, store, agent worker, connector
├── esaa/           # Intention orchestrator, contracts, projections
├── skills/         # Skill loader, matcher, manager
├── tools/          # All tool implementations
├── workflow/       # DAG planner and executor
├── sandbox/        # Bubblewrap + native sandbox
├── storage/        # SQLite + in-memory storage
├── policy.rs       # Shell command policy engine
├── audit.rs        # Audit logging
├── mcp.rs          # MCP server manager
├── scheduler.rs    # Cron scheduler
└── main.rs         # CLI commands and app wiring
```

## Architecture Decision Records

Significant decisions are documented in `adr/`. See [CLAUDE.md](CLAUDE.md) for the ADR workflow.

## Pull requests

- One feature or fix per PR
- Write a clear description of what changed and why
- Add tests for new functionality
- Update docs if you add user-facing features

## Reporting issues

Use GitHub Issues. Include:
- What you expected vs what happened
- Steps to reproduce
- OS, Rust version (`rustc --version`), zymi version (`zymi --version`)

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
