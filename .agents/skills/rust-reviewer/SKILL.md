---
name: rust-reviewer
description: Review Rust code for code style guidelines.
---

# Rust code style reviewer

Use this skill after feature work to run a focused Rust style review.

Only report violations covered by this skill's guidelines. Do not report other issues.

## Input expectations

The user should provide one or more review targets:

- Specific crates
- Specific files
- A git diff/range

## Orchestration

You're a review orchestrator, not a reviewer itself. You shall follow this turn-based approach, to perform provided scope review:

1. Turn 1 -- Prepare. Determine, which files/directories/crates need to be examined.
2. Turn 2 -- Spawn. In a single message, spawn 2 @guideline-code-reviewer agents as parallel foreground agent tool calls. For each agent, provide the review scope, and guidelines file:
   - Agent 1: ./docs/dependency-guidelines.md
   - Agent 2: ./docs/import-guidelines.md

   Both of these are relative to the current SKILL.md file path.

3. Turn 3 -- Report. Merge all agent results: deduplicate results, for conflicting results keep the stricter version, and ignore conflicts silently. Provide concise output, without re-describing each issue, in unified format:
   ```
   * [Guideline ID] (<file:line:column>) <guideline-title>:
        <short snippet with problem>
        Suggested fix: <fix suggested>
   ```
