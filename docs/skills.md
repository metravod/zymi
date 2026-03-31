# Skills

Skills are community-contributed extensions installed from git repositories. They add knowledge and/or tool capabilities to Zymi.

## Types of Skills

| Type | Source | Mechanism |
|------|--------|-----------|
| **Knowledge skill** | `skills/*/SKILL.md` | Injected into agent context when relevant |
| **Tool plugin** | `.mcp.json` in repo | Registered as MCP servers |

Both can coexist in a single plugin repository.

## Installing a Skill

The agent can install skills at runtime via the `manage_skills` tool:

```
manage_skills {"action": "install", "repo": "https://github.com/user/skills-repo"}
```

For monorepos with multiple plugins:

```
manage_skills {
  "action": "install",
  "repo": "https://github.com/user/skills-repo",
  "path": "plugins/my-skill"
}
```

This clones the repo into `memory/skills/`, discovers SKILL.md files and MCP configs, and registers everything.

## Managing Skills

| Action | Description |
|--------|-------------|
| `install` | Clone repo, discover skills and MCP servers |
| `list` | Show installed skills with versions |
| `update` | Git pull + re-discover skills |
| `remove` | Delete skill and clean up |
| `search` | Search installed skills by keyword |

## How Skills Work

1. On startup, `SkillManager` reads `memory/skills/registry.json` and scans for SKILL.md files
2. When a user sends a message, the matcher scores each skill's description against the message
3. Skills above the relevance threshold (keyword overlap) are injected as System messages
4. The agent sees the skill's instructions as part of its context and follows them
5. Maximum 3 skills injected per message to control context size

## SKILL.md Format

Skills use the Claude Code SKILL.md format — a markdown file with YAML frontmatter:

```markdown
---
name: my-skill
description: This skill should be used when the user asks to do X or work with Y
version: 1.0.0
---

# My Skill

Instructions for the agent when this skill is activated.

## Steps

1. First, do this
2. Then, do that
3. Finally, verify the result

## Reference Files

- **`references/patterns.md`** — Detailed patterns
- **`scripts/validate.sh`** — Validation utility
```

### Frontmatter Fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Skill identifier (kebab-case) |
| `description` | Yes | When this skill should activate. Write in third person: "This skill should be used when..." |
| `version` | No | Semver version |

### Writing Tips

- Keep the description specific — it controls when the skill activates
- Use imperative/infinitive form in the body ("Create a file", not "You should create a file")
- Keep the body under 3000 words. Move detailed content to `references/` subdirectory
- Reference supporting files explicitly so the agent knows they exist

## Directory Structure

### Plugin format (recommended)

```
my-plugin/
├── .claude-plugin/
│   └── plugin.json          # Optional metadata
├── skills/
│   └── my-skill/
│       ├── SKILL.md          # Required
│       ├── scripts/          # Executable helpers
│       ├── references/       # Additional docs
│       └── examples/         # Working examples
└── .mcp.json                 # Optional MCP servers
```

### Standalone format

```
my-skill/
└── SKILL.md                  # Required (at root level)
```

## Storage

Installed skills are stored in:

```
memory/skills/
├── registry.json              # Index of installed skills
└── repo-name/                 # Git clone of each repo
    └── ...
```

## Compatibility

Zymi's skill format is compatible with Claude Code's plugin system. Skills from Claude Code marketplaces can be installed directly.
