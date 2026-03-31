# Event-Driven Architecture & ESAA

## Event-Driven Architecture (EDA)

All message processing in Zymi flows through an event bus rather than direct method calls.

### Components

**EventBus** — In-process pub-sub system with bounded channels. Subscribers receive all published events.

**SqliteEventStore** — Append-only event persistence. Every domain event is stored with:
- `id` — unique event ID
- `stream_id` — correlation ID linking events in a request lifecycle
- `kind` — event type (serialized as JSON)
- `source` — which component emitted it
- `timestamp` — UTC timestamp

**EventDrivenConnector** — Bridge between connectors and the event bus:
- `submit_and_wait()` — publish `UserMessageReceived`, block until `ResponseReady`
- `submit_and_wait_streaming()` — same, but also registers a `StreamRegistry` sender for real-time token delivery

**AgentWorker** — Event consumer that processes inbound messages:
- Subscribes to `UserMessageReceived` events
- Calls `agent.process_stream()` with the message
- Publishes `ResponseReady` when done

**StreamRegistry** — Maps correlation IDs to streaming channels for real-time token delivery to CLI.

### Event Types

| Event | Description |
|-------|-------------|
| `UserMessageReceived` | User sent a message (text or multimodal) |
| `ResponseReady` | Agent finished processing, response available |
| `LlmCallStarted` | LLM API call initiated (includes context size) |
| `LlmCallCompleted` | LLM API call finished (includes usage, content preview) |
| `ToolCallRequested` | Tool call about to execute |
| `ToolCallCompleted` | Tool call finished (includes result preview, duration) |
| `IntentionEmitted` | Tool declared its intention via ESAA |
| `IntentionEvaluated` | Contract engine evaluated the intention |
| `ApprovalRequested` | Human approval needed |
| `ApprovalDecided` | Human responded to approval request |

### Data Flow

```
Connector → EventDrivenConnector.submit_and_wait()
                    │
                    ▼
              EventBus.publish(UserMessageReceived)
                    │
                    ▼
              AgentWorker receives event
                    │
                    ▼
              agent.process_stream(message, event_tx)
                    │
                    ├── LlmCallStarted / LlmCallCompleted (each iteration)
                    ├── ToolCallRequested / ToolCallCompleted (each tool)
                    ├── IntentionEmitted / IntentionEvaluated (ESAA tools)
                    │
                    ▼
              EventBus.publish(ResponseReady)
                    │
                    ▼
              Connector receives response
```

## ESAA (Event-Sourced Autonomous Agents)

ESAA adds a governance layer: tools declare their intended side-effects as structured data, which is validated by boundary contracts before execution.

### Intentions

Tools that modify state can implement `to_intention()`:

```rust
pub enum Intention {
    ExecuteShellCommand { command, timeout_secs },
    WriteFile { path, content },
    ReadFile { path },
    WebSearch { query },
    WebScrape { url },
    WriteMemory { key, content },
    SpawnSubAgent { name, task },
}
```

### Boundary Contracts

The `ContractEngine` evaluates intentions against rules:

**Shell Policy Contract** — delegates to the PolicyEngine:
- `allow` patterns → auto-approved
- `deny` patterns → rejected
- `require_approval` patterns → needs human confirmation
- Default: require approval

**File Write Contract** — path-based rules:
- `allowed_dirs` — directories where writes are permitted (e.g., `./memory/`, `/tmp/`)
- `deny_patterns` — file patterns always denied (e.g., `*.env`, `*.key`, `*.pem`)

### Verdicts

```rust
pub enum IntentionVerdict {
    Approved,                          // Auto-approved by contract
    RequiresHumanApproval { reason },  // Needs user confirmation
    Denied { reason },                 // Rejected by contract
}
```

### Orchestrator Pipeline

1. Tool's `to_intention()` converts arguments to an `Intention`
2. Orchestrator publishes `IntentionEmitted` event
3. ContractEngine evaluates against boundary contracts
4. Publishes `IntentionEvaluated` event
5. If `RequiresHumanApproval`: requests via ApprovalHandler, publishes `ApprovalRequested`/`ApprovalDecided`
6. Returns final verdict to the agent

### Hash Chain

Every intention evaluation is hash-chained for tamper-evident auditability. Each entry links to the previous via SHA-256, making it impossible to modify the audit trail without detection.

### Projections

Event projections reconstruct state from the event stream:
- **AuditProjection** — replays events to verify the audit trail
- Projections can be rebuilt from scratch by replaying stored events

### Which Tools Use ESAA?

Currently migrated:
- `execute_shell` → `ExecuteShellCommand`
- `write_memory` → `WriteMemory`
- `web_search` → `WebSearch`
- `web_scrape` → `WebScrape`

Tools without `to_intention()` fall back to the legacy `requires_approval()` mechanism.
