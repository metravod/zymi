# Engine Consolidation: Remove process_multimodal

Date: 2026-03-30

## Context

After EDA unification (ADR-0005), both CLI and Telegram route through `EventDrivenConnector → AgentWorker → process_stream`. The legacy `process_multimodal` method was identical to `process_stream` minus streaming events and workflow engine routing. It was only reachable via the `process()` wrapper, called by sub-agents, tasks, scheduler, planning, and eval — all text-only.

ADR-0005 noted: *"`process_multimodal` can be deprecated (no longer called)"*. This ADR executes that deprecation.

## Decision

1. **Delete `process_multimodal` (~160 lines)** and `process()` wrapper (~10 lines)
2. **Add `process_text()` (~10 lines)** — thin wrapper that creates a discarded event channel and delegates to `process_stream`
3. **Migrate all callers** from `process()` → `process_text()`:
   - `SpawnSubAgentTool.execute()` — sub-agent delegation
   - `SpawnTaskTool.execute()` — background task execution
   - `PlanningTool.run_simulation()` — feasibility simulations
   - `scheduler::execute_entry()` — cron-scheduled tasks
   - `eval::run_single_eval()` — evaluation framework
   - 4 unit tests in `agent.rs`

### Multimodal safety

Multimodal (image) support is unaffected:
- Telegram creates `Message::UserMultimodal` → routes through EDA → `process_stream` (unchanged)
- `prepare_messages()` stores the full multimodal message (unchanged)
- All LLM providers handle `UserMultimodal` (unchanged)
- Legacy callers only ever passed text strings

## Consequences

**Pros:**
- Single processing engine — `process_stream` is the only entry point
- ~160 lines of duplicated logic removed
- Sub-agents and tasks now get workflow engine routing (previously bypassed)
- All paths emit events for observability (even if discarded by the receiver)

**Cons:**
- Minimal overhead from unused event channel in `process_text` (channel allocation + immediate drop)

**Status of ADR-0005:** Fully realized — `process_multimodal` deprecated and removed.
