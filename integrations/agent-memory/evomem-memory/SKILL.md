---
name: evomem-memory
description: Use Evomem MCP to recall and checkpoint durable cross-session knowledge. Use for non-trivial work that may overlap earlier sessions, when the user asks what is remembered, before asking for context the user may already have shared, after a verified decision/fix/configuration/outcome, and when a compaction hook requests a memory checkpoint.
---

# Evomem Memory

Use the `memory_recall`, `memory_remember`, and `memory_forget` tools exposed by
the `evomem` MCP server.

## Recall

1. Before overlapping non-trivial work, call `memory_recall` with the project,
   task, and distinctive terms. Prefer `think` when synthesis or gaps matter;
   use `search` for a focused lookup.
2. Recall before asking the user to repeat likely prior context.
3. Treat recalled facts as leads. Reverify facts that can drift, especially
   branches, deployments, versions, credentials, live configuration, and
   external status.

## Checkpoint

At a durable milestone, let the LLM identify facts worth carrying to another
session. Milestones include an agreed decision or constraint, a verified root
cause or fix, a tested configuration or procedure, a stable preference, an
important path or command, and an unresolved blocker or risk.

For each independent fact:

1. Recall the same narrow topic first.
2. Skip the write when the fact is already present and unchanged.
3. When it is new or changed, call `memory_remember` with one concise,
   self-contained fact and 1-4 lowercase tags.

After a compaction checkpoint, recall the active project and task again before
continuing so the compacted session regains useful prior context.

## Boundaries

- Never save raw transcripts, raw logs, secrets, credentials, tokens, private
  keys, or unnecessary sensitive data.
- Do not save speculation, unverified hypotheses, transient progress, routine
  tool output, or duplicate summaries.
- Do not write a memory merely because the memory skill was invoked. Writing
  nothing is correct when no new durable fact exists.
- Use `memory_forget` only when the user explicitly asks to remove a memory.
