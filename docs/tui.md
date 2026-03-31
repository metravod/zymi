# CLI TUI

Zymi's CLI interface is a three-column terminal UI built with Ratatui.

## Layout

```
┌──────────────┬───────────────────────────┬──────────────────┐
│   Sidebar    │         Chat              │   Events Panel   │
│   (F1)       │                           │   (F2)           │
│              │  ┌─────────────────────┐  │                  │
│  Models      │  │ Conversation        │  │  LlmCallStarted │
│  > gpt-4.1   │  │ messages here...    │  │  ToolCallReq... │
│    claude     │  │                     │  │  IntentionEmit  │
│              │  │                     │  │  ToolCallComp.. │
│  Files       │  │                     │  │  LlmCallComp.. │
│  > AGENT.md  │  │                     │  │  ResponseReady  │
│              │  │                     │  │                  │
│  SubAgents   │  └─────────────────────┘  │                  │
│  > deployer  │  ┌─────────────────────┐  │                  │
│              │  │ Input area          │  │                  │
│  + Add model │  └─────────────────────┘  │                  │
├──────────────┴───────────────────────────┴──────────────────┤
│ F1:Sidebar  F2:Events  Esc:Interrupt  Ctrl+M:Model         │
└─────────────────────────────────────────────────────────────┘
```

## Starting the TUI

```bash
zymi cli
```

Panels auto-collapse below 100 columns terminal width.

## Keybindings

### Chat

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Shift+Enter` | New line in input |
| `Ctrl+Up` / `Ctrl+Down` | Scroll conversation |
| `PageUp` / `PageDown` | Scroll 10 lines |
| `Ctrl+Y` | Toggle copy mode (select text) |
| `Ctrl+M` | Open model selector |
| `Esc` | Interrupt running agent |

### Panels

| Key | Action |
|-----|--------|
| `F1` | Toggle sidebar (models, files, sub-agents) |
| `F2` | Toggle events panel (real-time observability) |
| `Tab` | Navigate sidebar items |
| `Enter` | Open selected file in `$EDITOR` |
| `Q` | Quit (only when sidebar is focused) |

### During Approval

When the agent requests approval (e.g., for a shell command):

| Key | Action |
|-----|--------|
| `Y` / `Enter` | Approve |
| `N` | Deny |
| `F1` / `F2` | Still work — panels remain accessible |
| `Esc` | Deny and interrupt agent |

## Sidebar (F1)

Three sections:

### Models
Lists configured LLM models. The active model is highlighted. Select and press Enter to switch.

The `+ Add model` entry at the bottom opens the setup flow for adding new providers.

### Files
Shows system files (`AGENT.md`, etc.) from the memory directory. Press Enter to open in `$EDITOR`.

### SubAgents
Lists created sub-agents. Press Enter to view/edit the sub-agent's system prompt.

## Events Panel (F2)

Real-time observability showing all domain events from the EventBus:

- **LlmCallStarted** — iteration number, message count, approximate context size
- **LlmCallCompleted** — whether tool calls were made, usage stats, content preview
- **ToolCallRequested** — tool name, arguments preview
- **ToolCallCompleted** — result preview, duration, error status
- **IntentionEmitted** / **IntentionEvaluated** — ESAA governance events
- **ApprovalRequested** / **ApprovalDecided** — human-in-the-loop events

Events are timestamped, word-wrapped, and scrollable. Each event shows an icon indicating its type.

## Logo

The header displays a gradient ZYMI logo (pink → purple → blue → teal) using Unicode block characters.

## Tips

- Use `F2` to monitor what the agent is doing in real-time — especially useful for debugging tool calls and understanding the agent's decision process
- `Esc` during agent processing interrupts cleanly — the agent stops after the current tool call
- `$EDITOR` defaults to `vim` if not set. Export `EDITOR=code` (or your preferred editor) for a different experience
