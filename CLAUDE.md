## 🏗️ Architecture Decision Records (ADR)
We use Architecture Decision Records in the `./adr/` directory to track important technical and structural decisions, avoid repeating discussions, and save tokens.

When working on architecture or making significant decisions, ALWAYS follow this workflow:

1. Scan Before Deciding:
   - Run `ls ./adr/` (or use Glob tool) to review the titles of existing decisions.
   - DO NOT read all ADR files. Only read the contents of specific files if their filenames directly relate to the current task.

2. Document New Decisions:
   - Create a new markdown file in `./adr/` named sequentially (e.g., `0005-use-polars-for-data.md`).
   - Use this exact format:
     - # [Title]
     - Date: [Current Date]
     - Context: [Why we need this]
     - Decision: [What we decided]
     - Consequences: [Pros, cons, and tech implications]

3. Update Existing Records:
   - If our new decision changes, overrides, or deprecates a previous decision, you must update the old ADR file.
   - Add a "Status" line to the old file (e.g., `Status: Superseded by 0005`) so our historical context remains accurate.

4. Notify: Tell me briefly in the chat when an ADR is created or updated.


## drift

This project is tracked by drift (.drift/project.json).

When working on this project:
- At session start: read .drift/project.json to understand current goals, status, and recent notes
- After completing significant work: add a note to .drift/project.json describing what was done
- When a goal is completed: set its "done" field to true and recalculate "progress"
- When new work is discovered: add it as a new goal
- Keep notes concise (one line, timestamp in ISO 8601 UTC)
- Update "lastActivity" on any change
