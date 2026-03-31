# Workflow Engine

The workflow engine handles complex multi-step tasks by generating and executing DAG (Directed Acyclic Graph) plans.

## When It Activates

Every user message is scored for complexity (0-10):

- **Score 0-5**: Simple task — handled by the standard agent loop
- **Score 6-10**: Complex task — routed to the workflow engine

Scoring uses heuristics (message length, keywords like "plan", "step-by-step", "create a project") with optional LLM-based assessment.

## Pipeline

```
User message
     │
     ▼
Assessment (heuristic + LLM)
     │
     ├── Simple → Agent loop (iterative tool use)
     │
     └── Complex ↓
              ▼
         DAG Planning (LLM generates plan)
              │
              ▼
         Plan Approval (user reviews)
              │
              ▼
         DAG Execution (parallel nodes)
              │
              ▼
         Synthesis (LLM combines results)
              │
              ▼
         Response
```

## DAG Planning

The LLM generates a structured plan with:

- **Nodes**: Individual tasks with tool calls
- **Dependencies**: Which nodes must complete before others can start
- **Descriptions**: What each node accomplishes

The plan is converted to a petgraph DAG for execution.

## DAG Execution

The `DagExecutor` runs independent nodes in parallel using `tokio::JoinSet`:

- Nodes with no dependencies start immediately
- Nodes wait until all their dependencies are resolved
- Results are cached — if a node fails and is retried, cached results from dependencies are reused
- Execution events stream to the CLI in real-time (visible in F2 events panel)

## Plan Approval

Before execution begins, the user sees the full plan and can:
- **Approve** — execution proceeds
- **Reject** — agent falls back to simple response mode
- **Modify** — (via conversation) ask the agent to adjust the plan

## Integration with EDA

Workflow execution flows through the same Event Bus as all other processing:
- `WorkflowStarted` / `WorkflowCompleted` events are published
- Individual tool calls within nodes go through the ESAA orchestrator
- All events are visible in the TUI events panel

## Configuration

No separate configuration file. Workflow behavior is controlled by:
- `max_iterations` in models.json settings — applies to both workflow and agent loop
- Tool availability — workflow uses the same tool registry as the agent
