## Architecture Decision Records (ADR)
Always create an Architecture Decision Record (ADR) whenever we make a significant architectural, structural, or technological decision during our session.

Follow these steps immediately after the decision is made:**
1. Create a new markdown file in the `./adr` directory.
2. Name the file sequentially with a descriptive slug (e.g., `0001-use-rust-for-backend.md`).
3. Format the file strictly using the standard ADR structure:
   - # [Short descriptive title]
   - Date:** [Current date]
   - Context: [Briefly explain the problem and why we needed to make this decision]
   - Decision: [What we specifically decided to do]
   - Consequences: [Pros, cons, and technical implications of this decision]
4. Notify me briefly in the chat that the ADR has been successfully created.



## drift

This project is tracked by drift (.drift/project.json).

When working on this project:
- At session start: read .drift/project.json to understand current goals, status, and recent notes
- After completing significant work: add a note to .drift/project.json describing what was done
- When a goal is completed: set its "done" field to true and recalculate "progress"
- When new work is discovered: add it as a new goal
- Keep notes concise (one line, timestamp in ISO 8601 UTC)
- Update "lastActivity" on any change
