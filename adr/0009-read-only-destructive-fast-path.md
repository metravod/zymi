# Read-Only / Destructive Tool Fast-Path

Date: 2026-04-01

## Context

The tool execution flow routes every tool call through approval logic (ESAA orchestrator or legacy `requires_approval`). For read-only tools like `web_search`, `current_time`, or `read_memory` this adds unnecessary latency and UX friction — they never mutate state and should never need approval.

Conversely, destructive tools (`execute_shell`, `run_code`) should always be flagged, even when auto-approve is enabled, so the system can log and guard them explicitly.

Claude Code uses a similar pattern with read-only fast-paths for its Read/Glob/Grep tools.

## Decision

Add two new default methods to the `Tool` trait:

- `is_read_only(&self) -> bool` — defaults to `false` (conservative). Tools that only read data override to `true`.
- `is_destructive(&self) -> bool` — defaults to `false`. Tools that perform hard-to-reverse operations override to `true`.

In `execute_tool_call`, read-only tools bypass both the ESAA orchestrator and the legacy approval path entirely — they go straight to `tool.execute()`. Destructive tools log a debug marker for observability.

### Tool classifications

| Classification | Tools |
|---|---|
| **Read-only** | `current_time`, `web_search`, `web_scrape`, `youtube_transcript`, `read_memory`, `think`, `check_task`, `list_tasks` |
| **Destructive** | `execute_shell`, `run_code`, `run_evals` |
| **Default** (neither) | All other tools — normal approval flow applies |

## Consequences

- **Reduced latency**: read-only tools skip orchestrator round-trip and approval handler entirely.
- **Better UX**: no approval prompts for harmless reads.
- **Explicit classification**: each tool declares its safety profile; makes the codebase self-documenting.
- **Foundation for concurrency**: `is_read_only()` tools are natural candidates for concurrent execution (future `is_concurrency_safe()` trait method).
- **Tradeoff**: a tool incorrectly marked `is_read_only` would bypass all safety checks. Conservative default (false) mitigates this.
