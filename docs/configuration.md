# Configuration

All config files live in the memory directory (`./memory` by default, `/opt/zymi/memory` for daemon installs).

## `models.json`

Model registry. If missing, falls back to `OPENAI_API_KEY` + `gpt-4.1-mini`.

```json
{
  "models": [
    {
      "id": "gpt-4.1-mini",
      "name": "GPT-4.1 Mini",
      "provider": "openai_compatible",
      "api_key_env": "OPENAI_API_KEY",
      "is_default": true,
      "input_price_per_1m": 0.40,
      "output_price_per_1m": 1.60
    },
    {
      "id": "claude-sonnet-4-20250514",
      "name": "Claude Sonnet 4",
      "provider": "anthropic",
      "api_key_env": "ANTHROPIC_API_KEY",
      "input_price_per_1m": 3.00,
      "output_price_per_1m": 15.00
    }
  ]
}
```

Fields:

- `provider`: `"openai_compatible"`, `"anthropic"`, or `"chatgpt_oauth"`
- `api_key_env`: env var name containing the API key
- `api_key`: alternative — hardcode the key directly (not recommended)
- `base_url`: custom API endpoint (for proxies, Azure, local models, etc.)
- `input_price_per_1m` / `output_price_per_1m`: optional, USD per 1M tokens — enables cost tracking in the CLI status line

## `policy.json`

Shell command policy. See [Security](security.md).

## `mcp.json`

[Model Context Protocol](https://modelcontextprotocol.io/) server configuration. Tools from MCP servers are auto-discovered and added as `{servername}_{toolname}`.

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/user/documents"],
      "env": {}
    },
    "remote-server": {
      "url": "http://localhost:3000/mcp"
    }
  }
}
```

- **Stdio transport**: `command` + `args` + optional `env`
- **HTTP transport**: `url`

## `AGENT.md`

System prompt for the main agent. Plain markdown. If missing, a generic default is used.

## `subagents/{name}.md`

System prompts for sub-agents. Created and managed by the bot via the `create_sub_agent` tool.

## Directory structure

```
memory/
├── AGENT.md               # Main agent system prompt
├── models.json            # LLM provider configuration (see models.json.example)
├── models.json.example    # Example model config (committed)
├── mcp.json               # MCP server configuration (see mcp.json.example)
├── mcp.json.example       # Example MCP config (committed)
├── policy.json            # Shell command policy rules
├── schedule.json          # Scheduled tasks (auto-managed)
├── auth.json              # OAuth tokens (ChatGPT)
├── audit.jsonl            # Append-only audit log
├── conversations.db       # Conversation history (SQLite)
├── subagents/
│   └── {name}.md          # Sub-agent prompts
├── evals/
│   └── {name}.json        # Eval test suites
├── eval_results/
│   └── {agent}_{ts}.json  # Eval run history
├── memory/
│   └── *.md               # Agent memory files (facts, preferences)
├── workflow_scripts/      # Generated tool scripts
└── workflow_traces/
    └── {ts}.json          # Workflow execution traces
```

## Environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `TELOXIDE_TOKEN` | **Daemon** | Telegram bot token from [@BotFather](https://t.me/BotFather) |
| `ALLOWED_USERS` | **Daemon** | Comma-separated Telegram user IDs |
| `OPENAI_API_KEY` | Yes* | API key for OpenAI-compatible provider |
| `ANTHROPIC_API_KEY` | Yes* | API key for Anthropic |
| `TAVILY_API_KEY` | No | Enables `web_search` tool ([Tavily](https://tavily.com/)) |
| `FIRECRAWL_API_KEY` | No | Enables `web_scrape` tool ([Firecrawl](https://firecrawl.dev/)) |
| `SUPADATA_API_KEY` | No | Enables `youtube_transcript` tool |
| `LANGFUSE_PUBLIC_KEY` | No | [LangFuse](https://langfuse.com/) observability (+ `LANGFUSE_SECRET_KEY`) |
| `MEMORY_DIR` | No | Path to memory directory (default: `./memory`) |
| `RUST_LOG` | No | Log level (e.g. `info`, `debug`) |

\* At least one LLM provider key is required. Configure via `models.json` or the setup wizard.
