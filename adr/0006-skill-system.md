# Skill System — Community Extensions via Git Repos

Date: 2026-03-30

## Context

ZYMI has a mature tool ecosystem (20+ built-in tools, MCP hot-reload, ESAA orchestrator) but no mechanism for community-contributed extensions. Claude Code has a proven plugin/skill format (SKILL.md + YAML frontmatter) that is simple, widely adopted, and language-agnostic. We want to support this format so ZYMI can use skills from existing repos (e.g. `artwist-polyakov/polyakov-claude-skills`) and allow users to create their own.

## Decision

### Skill Format: Claude Code Compatible

Skills use the Claude Code SKILL.md format:
- YAML frontmatter with `name`, `description`, `version`
- Markdown body with instructions/knowledge
- Optional subdirectories: `scripts/`, `references/`, `examples/`
- Optional `.mcp.json` for tool servers

### Architecture: Two Types, One Install Flow

| Type | Source | Mechanism |
|------|--------|-----------|
| Knowledge skill | `skills/*/SKILL.md` | Injected into agent context when relevant |
| Tool plugin | `.mcp.json` in repo | Registered as MCP servers |

### SkillManager

New `src/skills/` module with:
- `loader.rs` — git clone/pull, SKILL.md parsing, discovery
- `matcher.rs` — keyword overlap scoring with stop-word filtering
- `mod.rs` — SkillManager (init, install, update, remove, list, match_skills)

### ManageSkillsTool

Agent-facing tool (`manage_skills`) for runtime CRUD:
- `install` — git clone repo, discover skills + MCP, register
- `list` / `search` / `update` / `remove`

### Context Injection

In `Agent.prepare_messages()`, after facts/preferences, matched skills are injected as System messages. Matching uses keyword overlap between skill description and user message, with stop-word filtering and a 0.25 threshold. Max 3 skills per message.

### Storage Layout

```
memory/skills/
├── registry.json          # installed skills index
└── <repo-name>/           # git clone per repo
    └── skills/*/SKILL.md
```

## Consequences

**Pros:**
- Compatible with Claude Code's ecosystem — existing skills work out of the box
- Simple keyword matching works without external APIs (no embeddings needed)
- Git-based distribution — no custom registry infrastructure
- Hot-installable at runtime via ManageSkillsTool
- Supports monorepos (sub-path parameter for install)

**Cons:**
- Keyword matching is less precise than embedding-based (future enhancement)
- Full repo clone even for monorepos (sparse checkout optimization deferred)
- No progressive disclosure yet (full SKILL.md body injected, not tiered)
- Skills add to context window usage

**New files:**
- `src/skills/mod.rs`, `src/skills/loader.rs`, `src/skills/matcher.rs`
- `src/tools/manage_skills.rs`

**Modified files:**
- `src/core/agent.rs` — skill_manager field, prepare_messages injection
- `src/main.rs` — SkillManager init, ManageSkillsTool registration
- `src/tools/mod.rs` — module declaration
- `Cargo.toml` — serde_yaml dependency
