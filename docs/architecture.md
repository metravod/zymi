# Architecture

## Overview

Zymi is structured as an event-driven system with layered processing:

```
Connectors (CLI, Telegram)
    │
    ▼
EventDrivenConnector ──→ EventBus ──→ SQLite Event Store
    │
    ▼
AgentWorker
    │
    ├──→ Workflow Engine (complex tasks → DAG execution)
    │
    └──→ Agent Loop (simple tasks → iterative tool use)
              │
              ▼
         Tool Execution
              │
              ├──→ ESAA Orchestrator (intention validation)
              │         │
              │         ▼
              │    ContractEngine (policy + file contracts)
              │         │
              │         ▼
              │    Approval Handler (human-in-the-loop)
              │
              └──→ Direct execution (tools without intentions)
```

## Core Components

### Connectors

Entry points for user interaction. Both produce `Message` objects and route through the same EDA pipeline.

- **CLI TUI** (`src/connectors/cli/`) — Ratatui-based three-column layout. Registers a `StreamRegistry` sender for real-time token streaming. Uses `submit_and_wait_streaming` on the connector.
- **Telegram** (`src/connectors/telegram.rs`) — Teloxide-based bot. Handles text, photos (multimodal), and commands. Uses `submit_and_wait` on the connector.

### Event-Driven Architecture (EDA)

All message processing flows through an event bus:

1. **EventDrivenConnector** (`src/events/connector.rs`) — Publishes `UserMessageReceived` events and waits for `ResponseReady`.
2. **EventBus** (`src/events/bus.rs`) — In-process pub-sub with bounded channels. Subscribers receive all events.
3. **SqliteEventStore** (`src/events/store.rs`) — Append-only persistence. Every domain event is stored with correlation ID and timestamp.
4. **AgentWorker** (`src/events/agent_worker.rs`) — Subscribes to `UserMessageReceived`, calls `agent.process_stream()`, publishes `ResponseReady`.
5. **StreamRegistry** (`src/events/stream_registry.rs`) — Maps correlation IDs to streaming senders for real-time token delivery to CLI.

### Agent

The core processing engine (`src/core/agent.rs`):

- `process_stream()` — the single entry point for all processing. Accepts a `Message` (text or multimodal) and an event channel for streaming.
- `process_text()` — convenience wrapper for text-only callers (sub-agents, scheduler, etc.)
- `prepare_messages()` — builds the full context: system prompt → system info → preferences → facts → matched skills → conversation summary → history.

### Workflow Engine

Routes tasks based on complexity (`src/workflow/`):

1. **Assessment** — Heuristic or LLM scoring (0-10). Score > 5 triggers workflow.
2. **Planning** — LLM generates a DAG plan with tool nodes.
3. **DAG Building** — petgraph constructs the execution graph.
4. **Approval** — User approves before execution.
5. **Execution** — DagExecutor runs nodes in parallel via `tokio::JoinSet`.
6. **Synthesis** — LLM combines results into a response.

### ESAA (Event-Sourced Autonomous Agents)

Intention-based governance for tool execution (`src/esaa/`):

- Tools implement `to_intention()` to declare their side-effects as data.
- **ContractEngine** evaluates intentions against boundary contracts (shell policy, file write rules).
- **Orchestrator** coordinates: emit intention → evaluate → approve → execute.
- Hash chain provides tamper-evident audit trail.

### Tools

20+ built-in tools implementing the `Tool` trait (`src/tools/`):

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, arguments: &str) -> Result<String, String>;
    fn requires_approval(&self) -> bool { false }
    fn to_intention(&self, _arguments: &str) -> Option<Intention> { None }
}
```

Tools are dynamically registered via `Agent.register_tools()` and optionally selected via embedding-based RAG (`ToolSelector`).

### Skills

Community extensions from git repos (`src/skills/`):

- SKILL.md with YAML frontmatter → parsed and cached by `SkillManager`
- Matched against user messages by keyword relevance
- Injected into agent context as System messages

## Data Flow: User Message to Response

1. User types in CLI or sends Telegram message
2. Connector creates `Message::User` (text) or `Message::UserMultimodal` (with images)
3. `EventDrivenConnector.submit_and_wait()` publishes `UserMessageReceived` event
4. EventBus delivers event to AgentWorker
5. AgentWorker calls `agent.process_stream(message, event_tx)`
6. Workflow engine assesses complexity, routes to DAG or simple loop
7. Agent builds context (system prompt + preferences + facts + skills + history)
8. LLM generates response or tool calls
9. Tool calls route through ESAA orchestrator → contract evaluation → approval
10. Tool results feed back into the LLM loop
11. Final response published as `ResponseReady` event
12. Connector returns response to the user

## Module Map

```
src/
├── main.rs              # CLI commands, app builder, wiring
├── core/
│   ├── agent.rs         # Agent loop, process_stream, process_text
│   ├── openai.rs        # OpenAI provider
│   ├── anthropic.rs     # Anthropic provider
│   ├── chatgpt.rs       # ChatGPT OAuth provider
│   ├── provider_manager.rs  # Model selection and routing
│   ├── tool_selector.rs # Embedding-based tool selection (RAG)
│   ├── approval.rs      # Approval handler trait and shared slot
│   ├── config.rs        # Models and settings config
│   ├── history.rs       # Conversation history management
│   ├── extraction.rs    # Auto fact extraction
│   └── langfuse.rs      # LangFuse observability
├── connectors/
│   ├── cli/             # TUI (app, ui, input modules)
│   └── telegram.rs      # Telegram bot
├── events/
│   ├── bus.rs           # Event bus (pub-sub)
│   ├── store.rs         # SQLite event store
│   ├── agent_worker.rs  # Event consumer
│   ├── connector.rs     # EventDrivenConnector
│   └── stream_registry.rs  # Streaming support
├── esaa/
│   ├── orchestrator.rs  # Intention orchestrator
│   ├── contracts.rs     # Boundary contracts
│   └── projections.rs   # Event projections
├── skills/
│   ├── loader.rs        # Git + SKILL.md parsing
│   ├── matcher.rs       # Keyword matching
│   └── mod.rs           # SkillManager
├── tools/               # 20+ tool implementations
├── workflow/            # DAG planner + executor
├── sandbox/             # Bubblewrap + native sandbox
├── policy.rs            # Shell command policy engine
├── audit.rs             # Audit logging
├── scheduler.rs         # Cron scheduler
├── storage/             # SQLite + in-memory storage
└── mcp.rs               # MCP server manager
```
