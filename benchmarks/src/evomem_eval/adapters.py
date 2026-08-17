from __future__ import annotations

import hashlib
import json
import re
import urllib.request
from collections.abc import Iterable
from pathlib import Path
from typing import Any

from .models import Event, Scenario

LONGMEMEVAL_REVISION = "98d7416c24c778c2fee6e6f3006e7a073259d48f"
LOCOMO_REVISION = "3eb6f2c585f5e1699204e3c3bdf7adc5c28cb376"
MEMORY_AGENT_REVISION = "7ea066982b140a19337e17e60d45d4076e042faf"
LONGMEMEVAL_URL = f"https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/{LONGMEMEVAL_REVISION}/longmemeval_s_cleaned.json"
LOCOMO_URL = f"https://raw.githubusercontent.com/snap-research/locomo/{LOCOMO_REVISION}/data/locomo10.json"
MEMORY_AGENT_DATASET = "ai-hyz/MemoryAgentBench"


def internal_scenarios() -> list[Scenario]:
    rows = [
        (
            "single",
            "single-author parity",
            [("e1", "alice", "The Atlas launch color is cobalt blue.")],
            "What is the Atlas launch color?",
            "cobalt blue",
            ("e1",),
            False,
            "",
        ),
        (
            "cross",
            "cross-author retrieval",
            [("e1", "alice", "Project Juniper deploys every Tuesday.")],
            "When does Project Juniper deploy?",
            "Tuesday",
            ("e1",),
            False,
            "",
        ),
        (
            "provenance",
            "provenance",
            [("e1", "bob", "Bob approved the Quartz migration.")],
            "Who approved the Quartz migration?",
            "Bob",
            ("e1",),
            False,
            "",
        ),
        (
            "duplicate",
            "duplicates",
            [
                ("e1", "alice", "The Vega API uses port 7443."),
                ("e2", "bob", "The Vega API uses port 7443."),
            ],
            "Which port does the Vega API use?",
            "7443",
            ("e1", "e2"),
            False,
            "",
        ),
        (
            "conflict",
            "conflicts",
            [
                ("e1", "alice", "Alice says the Orion freeze starts Friday."),
                ("e2", "bob", "Bob says the Orion freeze starts Saturday."),
            ],
            "When does the Orion freeze start, and do sources disagree?",
            "Alice says Friday; Bob says Saturday; the sources conflict.",
            ("e1", "e2"),
            False,
            "",
        ),
        (
            "update",
            "updates",
            [
                ("e1", "alice", "The Nova budget was 12k."),
                ("e2", "alice", "The Nova budget is now 15k, replacing 12k."),
            ],
            "What is the current Nova budget?",
            "15k",
            ("e2",),
            False,
            "",
        ),
        (
            "temporal",
            "temporal",
            [
                ("e1", "alice", "On 2026-01-02 the Cedar review happened."),
                ("e2", "bob", "On 2026-02-03 the Cedar review happened again."),
            ],
            "When was the latest Cedar review?",
            "2026-02-03",
            ("e2",),
            False,
            "",
        ),
        (
            "abstain",
            "abstention",
            [("e1", "alice", "The Birch owner is Dana.")],
            "What is the Birch database password?",
            "The memory does not contain that information.",
            (),
            True,
            "",
        ),
        (
            "own-forget",
            "own forget",
            [("e1", "alice", "Temporary Kilo token is retired.")],
            "Is the retired Kilo token still remembered?",
            "No",
            (),
            True,
            "own_forget",
        ),
        (
            "cross-forget",
            "blocked cross forget",
            [("e1", "alice", "Lima policy must remain available.")],
            "What must remain available?",
            "Lima policy",
            ("e1",),
            False,
            "cross_forget",
        ),
    ]
    return [
        Scenario(
            id=f"internal-{key}",
            suite="internal",
            category=category,
            events=tuple(
                Event(f"{key}-{i}", a, text, f"2026-01-{n + 1:02d}T00:00:00Z")
                for n, (i, a, text) in enumerate(events)
            ),
            question=question,
            answer=answer,
            evidence_ids=tuple(f"{key}-{i}" for i in evidence),
            abstain=abstain,
            operation=operation,
            memory_id="internal-shared",
        )
        for key, category, events, question, answer, evidence, abstain, operation in rows
    ]


def _download(url: str, target: Path) -> tuple[Path, str]:
    target.parent.mkdir(parents=True, exist_ok=True)
    if not target.exists():
        with urllib.request.urlopen(url, timeout=60) as response:
            target.write_bytes(response.read())
    return target, hashlib.sha256(target.read_bytes()).hexdigest()


def _load(path: Path) -> Any:
    text = path.read_text()
    if path.suffix == ".jsonl":
        return [json.loads(line) for line in text.splitlines() if line.strip()]
    return json.loads(text)


def _generic(rows: Iterable[dict[str, Any]], suite: str) -> list[Scenario]:
    out: list[Scenario] = []
    for n, row in enumerate(rows):
        events_raw = (
            row.get("events") or row.get("memories") or row.get("context") or []
        )
        if isinstance(events_raw, str):
            events_raw = [{"text": events_raw}]
        events = tuple(
            Event(
                str(item.get("id", f"e{i + 1}")),
                str(
                    item.get("author") or item.get("speaker") or f"speaker-{i % 2 + 1}"
                ),
                str(
                    item.get("text")
                    or item.get("content")
                    or item.get("memory")
                    or item
                ),
                str(item.get("timestamp") or item.get("date") or ""),
            )
            for i, item in enumerate(events_raw)
        )
        evidence = row.get("evidence_ids") or row.get("evidence") or []
        if isinstance(evidence, str):
            evidence = [evidence]
        out.append(
            Scenario(
                id=str(row.get("id") or row.get("question_id") or f"{suite}-{n + 1}"),
                suite=suite,
                category=str(row.get("category") or row.get("question_type") or "qa"),
                events=events,
                question=str(row.get("question") or row.get("query") or ""),
                answer=str(row.get("answer") or row.get("expected") or ""),
                evidence_ids=tuple(map(str, evidence)),
                abstain=bool(row.get("abstain", False)),
                memory_id=str(row.get("memory_id", "")),
            )
        )
    return [x for x in out if x.events and x.question]


def _longmemeval(rows: list[dict[str, Any]]) -> list[Scenario]:
    normalized = []
    for row in rows:
        events = []
        sessions = row.get("haystack_sessions") or row.get("sessions") or []
        dates = row.get("haystack_dates") or row.get("session_dates") or []
        ids = row.get("haystack_session_ids") or row.get("session_ids") or []
        for i, session in enumerate(sessions):
            turns = session if isinstance(session, list) else [session]
            text = "\n".join(
                f"{t.get('role', t.get('speaker', 'speaker'))}: {t.get('content', t.get('text', t))}"
                if isinstance(t, dict)
                else str(t)
                for t in turns
            )
            events.append(
                {
                    "id": str(ids[i] if i < len(ids) else f"session-{i + 1}"),
                    "author": "conversation",
                    "text": text,
                    "timestamp": str(dates[i] if i < len(dates) else ""),
                }
            )
        normalized.append(
            {
                "id": row.get("question_id"),
                "category": row.get("question_type"),
                "events": events,
                "question": row.get("question"),
                "answer": row.get("answer"),
                "evidence_ids": row.get("answer_session_ids", []),
            }
        )
    return _generic(normalized, "longmemeval")


def _locomo(rows: list[dict[str, Any]]) -> list[Scenario]:
    out = []
    for conversation_n, row in enumerate(rows):
        conversation = row.get("conversation", row)
        events = []
        for key, session in (
            conversation.items() if isinstance(conversation, dict) else []
        ):
            if not str(key).startswith("session_") or str(key).endswith("_date_time"):
                continue
            timestamp = conversation.get(f"{key}_date_time", "")
            for i, turn in enumerate(session if isinstance(session, list) else []):
                if isinstance(turn, dict):
                    events.append(
                        {
                            "id": str(turn.get("dia_id", f"{key}-{i}")),
                            "author": str(turn.get("speaker", "speaker")),
                            "text": str(turn.get("text", turn.get("content", ""))),
                            "timestamp": str(timestamp),
                        }
                    )
        for qa_n, qa in enumerate(row.get("qa", [])):
            out.append(
                {
                    "id": f"locomo-{conversation_n + 1}-{qa_n + 1}",
                    "memory_id": f"locomo-conversation-{conversation_n + 1}",
                    "category": qa.get("category", "qa"),
                    "events": events,
                    "question": qa.get("question"),
                    "answer": qa.get("answer"),
                    "evidence_ids": qa.get("evidence", []),
                }
            )
    return _generic(out, "locomo")


def _maybe_json(value: Any) -> Any:
    if isinstance(value, str):
        try:
            return json.loads(value)
        except json.JSONDecodeError:
            return value
    return value


def _memoryagentbench(rows: list[dict[str, Any]]) -> list[Scenario]:
    out = []
    for row_n, row in enumerate(rows):
        context = _maybe_json(row.get("context", ""))
        questions = _maybe_json(row.get("questions", []))
        answers = _maybe_json(row.get("answers", []))
        metadata = _maybe_json(row.get("metadata", {})) or {}
        if isinstance(questions, str):
            questions = [questions]
        if not isinstance(answers, list):
            answers = [answers]
        ids = metadata.get("qa_pair_ids") or metadata.get("question_ids") or []
        blocks = [
            block.strip()
            for block in re.split(
                r"(?=^(?:Document|Dialogue) \d+:)", str(context), flags=re.MULTILINE
            )
            if block.strip()
        ]
        if len(blocks) == 1:
            blocks = [
                str(context)[start : start + 4000]
                for start in range(0, len(str(context)), 4000)
            ]
        events = [
            {
                "id": f"context-{row_n + 1}-{i + 1}",
                "author": "participant",
                "text": block,
            }
            for i, block in enumerate(blocks)
        ]
        for question_n, question in enumerate(questions):
            answer = answers[question_n] if question_n < len(answers) else ""
            if isinstance(answer, list):
                answer = answer[0] if answer else ""
            out.append(
                {
                    "id": str(
                        ids[question_n]
                        if question_n < len(ids)
                        else f"memoryagentbench-{row_n + 1}-{question_n + 1}"
                    ),
                    "category": row.get("_split", "qa"),
                    "memory_id": f"memoryagentbench-row-{row_n + 1}",
                    "events": events,
                    "question": str(question),
                    "answer": str(answer),
                    "evidence_ids": [],
                }
            )
    return _generic(out, "memoryagentbench")


def load_suite(
    name: str, profile: str, cache: Path, data_path: Path | None = None
) -> tuple[list[Scenario], dict[str, str]]:
    if name == "internal":
        return internal_scenarios(), {"source": "committed synthetic v1"}
    if profile == "smoke" and data_path is None:
        data_path = Path(__file__).parents[2] / "fixtures" / f"{name}.json"
    if data_path:
        return _generic(_load(data_path), name), {
            "source": str(data_path),
            "sha256": hashlib.sha256(data_path.read_bytes()).hexdigest(),
        }
    if name == "longmemeval":
        path, digest = _download(LONGMEMEVAL_URL, cache / "longmemeval.json")
        return _longmemeval(_load(path)), {
            "source": LONGMEMEVAL_URL,
            "revision": LONGMEMEVAL_REVISION,
            "sha256": digest,
        }
    if name == "locomo":
        path, digest = _download(LOCOMO_URL, cache / "locomo10.json")
        return _locomo(_load(path)), {
            "source": LOCOMO_URL,
            "revision": LOCOMO_REVISION,
            "sha256": digest,
        }
    if name == "memoryagentbench":
        from datasets import load_dataset

        dataset = load_dataset(
            MEMORY_AGENT_DATASET,
            revision=MEMORY_AGENT_REVISION,
            cache_dir=str(cache / "huggingface"),
        )
        rows = [
            dict(row) | {"_split": split_name}
            for split_name, split in dataset.items()
            for row in split
        ]
        return _memoryagentbench(rows), {
            "source": MEMORY_AGENT_DATASET,
            "revision": MEMORY_AGENT_REVISION,
        }
    raise ValueError(f"unknown suite: {name}")
