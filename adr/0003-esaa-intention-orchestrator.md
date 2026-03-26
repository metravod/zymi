# ESAA: Intention-based orchestrator with hash chain verification

**Date:** 2026-03-26

## Context

With EDA in place (Phase A), the agent still executes side effects directly — tool calls go straight to `tool.execute()`. The ESAA paper (arXiv:2602.23193) proposes a governance layer: agents emit *intentions* (what they want to do), a deterministic orchestrator validates them against boundary contracts, and all decisions are recorded in a tamper-evident event log.

We needed a way to:
1. Validate agent actions against security contracts before execution
2. Create an immutable, verifiable audit trail
3. Enable replay and forensic analysis of agent behavior

## Decision

1. **Intention types**: `Intention` enum (ExecuteShellCommand, WriteFile, ReadFile, WebSearch, WebScrape, WriteMemory, SpawnSubAgent) — each tool call maps to an intention via `Tool::to_intention()`.

2. **ContractEngine**: Wraps existing `PolicyEngine` for shell commands, adds file write contracts (allowed_dirs, deny_patterns). Returns `IntentionVerdict` (Approved, RequiresHumanApproval, Denied).

3. **Orchestrator**: Deterministic pipeline — emit IntentionEmitted event, evaluate contracts, emit IntentionEvaluated, handle approval if needed, return verdict. Does NOT execute side effects (caller's responsibility).

4. **Hash chain**: SHA-256(event_id + data + prev_hash) per stream. `verify_chain()` replays and validates continuity. Tamper-evident — any modification breaks the chain.

5. **Projections**: `Projection` trait with `apply(event)` for deterministic state rebuild. ConversationProjection rebuilds messages, MetricsProjection aggregates usage stats.

## Consequences

**Pros:**
- Every agent action has a verifiable audit trail
- Security contracts are evaluated before execution, not after
- Hash chain makes event tampering detectable
- Projections enable replay-based debugging and state verification
- Gradual migration: tools without `to_intention()` use legacy path

**Cons:**
- Adds indirection to tool execution path (intention -> orchestrator -> execute)
- Hash chain verification is O(n) per stream (acceptable for conversation-length streams)
- ContractEngine currently has no rate limiting (placeholder config exists)
