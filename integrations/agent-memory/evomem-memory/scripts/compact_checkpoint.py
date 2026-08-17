#!/usr/bin/env python3
"""Inject an LLM-driven Evomem checkpoint after context compaction."""

import json
import sys


def main() -> None:
    try:
        event = json.load(sys.stdin)
    except (json.JSONDecodeError, OSError):
        return

    if event.get("hook_event_name") != "SessionStart" or event.get("source") != "compact":
        return

    context = (
        "Run the evomem-memory compaction checkpoint now. Let the LLM inspect "
        "the compacted context, call memory_recall for the active project/task "
        "and each candidate fact, and call memory_remember only for verified, "
        "durable facts that are new or changed. Never save the raw summary, "
        "transcript, secrets, hypotheses, or transient progress. If nothing is "
        "new, write nothing. Recall the active task state again, then continue it."
    )
    print(
        json.dumps(
            {
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": context,
                }
            }
        )
    )


if __name__ == "__main__":
    main()
