# Tool Prompt System

Date: 2026-04-01

## Context

Tool descriptions sent to the LLM API serve double duty: they describe **what** a tool does (for tool selection) and **how** to use it well (behavioral guidance). Packing both into the `description` field made descriptions bloated and hard to maintain. We needed a clean separation between the tool's identity and its usage guidelines.

## Decision

Add an optional `fn prompt(&self) -> Option<String>` method to the `Tool` trait with a default `None` implementation. At API call time (in `Agent::get_tool_definitions` and `WorkflowExecutor`), the prompt is appended to the description with a double newline separator.

This keeps `definition()` focused on the tool's identity and parameters, while `prompt()` carries rich behavioral guidelines: when to use vs. alternatives, safety constraints, best practices.

Initial prompts added for 7 tools: `execute_shell`, `run_code`, `web_search`, `web_scrape`, `write_memory`, `think`, `ask_user`.

## Consequences

- **Pro:** Tool descriptions stay concise; behavioral guidance is separate and maintainable.
- **Pro:** Zero-cost for tools that don't need prompts (default returns `None`).
- **Pro:** Prompts can reference other tools by name, guiding the LLM toward better tool selection.
- **Con:** Slightly longer API payloads for tools with prompts (but well within token budgets).
- **Con:** Need to keep prompts up to date as tool behavior changes.
