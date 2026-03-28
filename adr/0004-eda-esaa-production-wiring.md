# Wire EDA + ESAA into the production message flow

**Date:** 2026-03-27

## Context

EDA (ADR-0001) and ESAA (ADR-0003) were implemented as standalone modules but ran "alongside" the real request path — connectors still called `agent.process_multimodal()` directly, and `execute_tool_call` bypassed the Orchestrator. Events were emitted as side-effects, not as the primary flow.

To realize the architectural benefits (decoupling, audit, contract enforcement), the production path must route through the event bus and orchestrator.

## Decision

### 1. Telegram routes through EventDrivenConnector

`gpt_handler()` now calls `EventDrivenConnector.submit_and_wait()` instead of `agent.process_multimodal()`. The flow becomes:

```
Telegram message
  → EventDrivenConnector.submit_and_wait()
    → publishes UserMessageReceived to EventBus
    → AgentWorker picks up event, calls agent.process_multimodal()
    → publishes ResponseReady with matching correlation_id
  → connector receives ResponseReady, returns to Telegram
```

Timeout: 600s (generous — agent may run multi-iteration tool loops).

### 2. Tool calls route through Orchestrator

`execute_tool_call()` now checks `tool.to_intention()` first:
- If `Some(intention)` AND orchestrator is present → `Orchestrator.process_intention()` evaluates boundary contracts, handles approval, emits events, returns verdict.
- If `None` (tool not migrated) OR no orchestrator → legacy path unchanged (direct `requires_approval_for` + execute).

This is backwards-compatible: tools without `to_intention()` (11 of 15) still use the old path.

### 3. CLI stays on legacy path (streaming)

CLI uses `process_stream()` with an `mpsc` channel for real-time TUI updates. `EventDrivenConnector` only supports request-response (returns final string). CLI still benefits from ESAA via the Orchestrator in `execute_tool_call`, but message routing stays direct.

Future: add streaming support to EDA (stream events through the bus) or use a hybrid approach.

### 4. Orchestrator bootstrapped in main.rs

`ContractEngine` is created from the existing `PolicyEngine` + a `FileWriteContract` (allowed_dirs: `./memory/`, `/tmp/`; deny: `*.env`, `*.key`, `*.pem`). The `Orchestrator` wraps the contract engine and event bus.

## Consequences

**Pros:**
- Telegram messages are now fully event-driven — every request/response is an event in the store
- Tool calls for shell, web_search, web_scrape, write_memory go through contract evaluation before execution
- Crash recovery works: orphaned UserMessageReceived events (no ResponseReady) are replayed on restart
- ApprovalSlotGuard pattern preserved — connector sets the handler, AgentWorker reads it

**Cons:**
- Telegram has an extra hop (connector → bus → worker → agent → bus → connector) adding minor latency
- CLI doesn't participate in event-driven message routing yet (only tool-level ESAA)
- FileWriteContract config is hardcoded in main.rs (should move to config file)
