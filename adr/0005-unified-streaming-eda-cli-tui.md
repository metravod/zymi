# Unified Streaming EDA + Three-Column CLI TUI

Date: 2026-03-28

## Context

After wiring Telegram through EDA (ADR-0004), CLI remained on a direct `agent.process_stream()` path — bypassing EventBus, AgentWorker, and ESAA orchestration entirely. This created two divergent processing engines:

- **Telegram**: `EventDrivenConnector → EventBus → AgentWorker → process_multimodal`
- **CLI**: direct `agent.process_stream()` with no event persistence

Additionally, the CLI TUI was a single-column layout with no visibility into agent internals (intentions, contracts, tool lifecycle events).

## Decision

### 1. Unified Engine via StreamRegistry

Introduce `StreamRegistry` — a shared map of `correlation_id → UnboundedSender<StreamEvent>`. This bridges the EDA path with real-time streaming:

- CLI registers its `stream_tx` in the registry before publishing `UserMessageReceived`
- AgentWorker takes the sender from the registry and passes it to `process_stream`
- StreamEvents flow directly from the agent to CLI without persisting tokens to SQLite
- Domain events (LlmCallStarted, ToolCallCompleted, etc.) still flow through EventBus normally

### 2. AgentWorker Switches to `process_stream`

AgentWorker now always calls `process_stream` instead of `process_multimodal`. This gives all connectors (Telegram included) access to the workflow engine routing that was previously CLI-only.

For Telegram (no registered stream sender), a throwaway channel is created — StreamEvents are silently discarded since Telegram only needs the final `ResponseReady`.

### 3. `submit_and_wait_streaming` on EventDrivenConnector

New method that combines stream registration with the existing `submit_and_wait` pattern. CLI spawns a task that:
1. Sets approval handler via `ApprovalSlotGuard`
2. Calls `submit_and_wait_streaming` (blocks until `ResponseReady`)
3. Guard stays alive for the duration of processing

The existing `submit_and_wait` remains unchanged for Telegram.

### 4. Three-Column CLI TUI

- **Left panel** (F1): Models, system files (AGENT.md), subagents — with navigation and `$EDITOR` integration
- **Center**: Existing chat UI (header, messages, input) + hint bar
- **Right panel** (F2): Real-time EventBus observability — shows all domain events with timestamps, icons, and details
- Panels auto-collapse below 100 columns terminal width
- Hint bar at bottom shows context-sensitive keybindings

## Consequences

**Pros:**
- Single processing engine for all connectors — consistent behavior, single code path to maintain
- Telegram gains workflow engine routing (was previously CLI-only)
- CLI gains full EDA/ESAA observability via the right panel
- StreamEvents don't pollute the SQLite event store (only domain events are persisted)
- `process_multimodal` can be deprecated (no longer called)

**Cons:**
- StreamRegistry adds a shared mutable state component
- AgentWorker creates throwaway channels for non-streaming paths (minor overhead)
- Three-column TUI increases UI code complexity (~200 additional lines)

**New files:**
- `src/events/stream_registry.rs` — StreamRegistry (4 tests)

**Modified files:**
- `src/events/agent_worker.rs` — StreamRegistry field, process_stream
- `src/events/connector.rs` — StreamRegistry field, submit_and_wait_streaming
- `src/core/agent.rs` — process_stream signature: `&str` → `Message`
- `src/connectors/cli/mod.rs` — EDA wiring, EventBus subscriber, $EDITOR
- `src/connectors/cli/app.rs` — Panel state, ObservabilityEntry, handle_domain_event
- `src/connectors/cli/ui.rs` — Three-column layout, left/right panels, hint bar
- `src/connectors/cli/input.rs` — F1/F2/Tab keybindings, OpenEditor action
- `src/connectors/telegram.rs` — StreamRegistry parameter passthrough
- `src/main.rs` — StreamRegistry creation, CLI connector wiring
