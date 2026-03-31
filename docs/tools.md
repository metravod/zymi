# Tools

Zymi includes 20+ built-in tools. Additional tools can be added via MCP servers or the skill system.

## Communication

### `ask_user`
Ask the user a question and wait for their response. Used when the agent needs clarification or approval for a decision.

### `current_time`
Returns the current date and time. Useful for time-aware tasks and scheduling.

## Shell & Code Execution

### `execute_shell`
Execute shell commands. All commands pass through the [policy engine](security.md) and optionally through the [ESAA orchestrator](eda-esaa.md) for intention-based validation.

- Supports timeout configuration
- Commands can be sandboxed via bubblewrap
- Requires approval by default (configurable via policy.json)

### `run_code`
Write and execute code in Python, Bash, or Node.js. Code is written to a temporary file and executed in a subprocess. Supports optional bubblewrap sandboxing.

## Memory

### `read_memory`
Read files from the `memory/` directory. The agent uses this to recall stored information, preferences, and context.

### `write_memory`
Write or update files in the `memory/` directory. Supports git auto-sync when enabled.

## Agent Delegation

### `create_sub_agent`
Create a reusable sub-agent with a custom system prompt (stored in `memory/subagents/{name}.md`). Sub-agents can be specialized for specific domains.

### `spawn_sub_agent`
Delegate a task to a named sub-agent. The sub-agent runs synchronously with its own conversation context and a subset of parent tools. Max nesting depth: 3.

### `spawn_task` / `check_task` / `list_tasks`
Background async task management. Tasks run in parallel and their results are stored in the task registry. Useful for long-running operations that shouldn't block the main conversation.

### `planning`
Structured reasoning tool. Generates multiple approaches, runs sub-agent feasibility simulations in parallel, and selects the best path. Used for complex decision-making.

## Scheduling

### `manage_schedule`
CRUD for cron-like scheduled tasks. Tasks are stored in `memory/schedule.json` and executed by the scheduler daemon. Each task runs with its own agent instance.

## Web & Search

### `web_search`
Web search powered by [Tavily](https://tavily.com/). Requires `TAVILY_API_KEY`.

### `web_scrape`
Web page scraping via [Firecrawl](https://firecrawl.dev/). Extracts clean text content from URLs. Requires `FIRECRAWL_API_KEY`.

### `youtube_transcript`
Extract YouTube video transcripts via Supadata API. Requires `SUPADATA_API_KEY`.

## System Management

### `manage_mcp`
Manage MCP (Model Context Protocol) servers at runtime:
- `list` — show configured servers
- `add` — register a new server (auto hot-reload)
- `remove` — unregister a server

### `manage_skills`
Manage community skill extensions:
- `install` — clone from git repo, discover SKILL.md files
- `list` — show installed skills
- `update` — pull latest changes
- `remove` — uninstall
- `search` — find skills by keyword

### `manage_policy`
View and modify shell command policy rules at runtime.

## Evaluation

### `generate_evals`
Auto-generate evaluation test suites for sub-agents. Creates test cases with expected behaviors.

### `run_evals`
Execute evaluation suites with multi-dimensional LLM judge scoring. Supports multiple runs for statistical reliability.

## Tool Trait

All tools implement:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, arguments: &str) -> Result<String, String>;

    // Optional: policy-based approval
    fn requires_approval(&self) -> bool { false }
    fn requires_approval_for(&self, _arguments: &str) -> bool { self.requires_approval() }

    // Optional: ESAA intention for orchestrator validation
    fn to_intention(&self, _arguments: &str) -> Option<Intention> { None }
}
```

Tools with `to_intention()` participate in the ESAA governance system — their side-effects are validated by boundary contracts before execution.

## Tool Selection

When many tools are available (20+ built-in + MCP), Zymi uses embedding-based RAG to select the most relevant tools per query. This is controlled by the `tool_selection` setting in `models.json` and requires `OPENAI_API_KEY` for embeddings.

Always-available tools (not subject to selection): `think`, `ask_user`, `get_current_time`, `read_memory`.
