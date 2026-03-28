# Adopt Event-Driven Architecture with ESAA governance layer

**Date:** 2026-03-26
**Status:** Implemented. Wired into production path in ADR-0004.

## Context

Zymi uses synchronous request-response: connectors (Telegram, CLI) call `agent.process_multimodal()` directly, block until the LLM finishes, and return the result. This creates tight coupling between connectors and the agent, makes adding new event sources difficult, provides no audit trail, and makes replay/debugging impossible.

Two directions were evaluated:
1. **Research Bench** — a separate experiment runner for comparing agent architectures from academic papers. High visibility but narrow audience, and it creates a parallel runtime alongside the existing one.
2. **Event-Driven Architecture (EDA) + ESAA** — restructure the core to be event-driven, then layer governance (agents as intention emitters, boundary contracts, immutable audit, replay) inspired by the ESAA paper (arXiv:2602.23193).

## Decision

Adopt EDA as the core architectural pattern, then layer ESAA governance on top. Implementation in two phases:

**Phase A (EDA):** Events module (`src/events/`) with Event types, SqliteEventStore (append-only table in existing `conversations.db`), in-process EventBus, AgentWorker as event consumer, connectors as event producers.

**Phase B (ESAA):** Intention types, ContractEngine (wrapping existing PolicyEngine), deterministic Orchestrator, `to_intention()` on Tool trait, hash chain + replay.

Migration is incremental — old and new paths coexist via `Option` fields. No big-bang rewrite.

## Consequences

**Pros:**
- Decouples connectors from agent — adding new event sources (webhooks, cron, GitHub events) is trivial
- Immutable event log provides audit trail and time-travel debugging for free
- Crash recovery: orphaned events can be detected and replayed on restart
- Boundary contracts generalize the existing shell-only PolicyEngine to all agent actions
- Strong portfolio/positioning story ("event sourcing for LLM agents")

**Cons:**
- Every tool call now involves a DB write (mitigated by SQLite WAL mode)
- Increased complexity in the processing pipeline (event → worker → agent → event)
- Dual-write period during migration (both direct calls and events)
- Bounded channels may drop events for slow subscribers (mitigated by store-first persistence + replay)
