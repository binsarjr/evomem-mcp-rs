from __future__ import annotations

from dataclasses import asdict, dataclass, field
from typing import Any


@dataclass(frozen=True)
class Event:
    id: str
    author: str
    text: str
    timestamp: str = ""


@dataclass(frozen=True)
class Scenario:
    id: str
    suite: str
    category: str
    events: tuple[Event, ...]
    question: str
    answer: str
    evidence_ids: tuple[str, ...]
    abstain: bool = False
    operation: str = ""
    memory_id: str = ""

    def json(self) -> dict[str, Any]:
        return asdict(self)


@dataclass
class CaseResult:
    scenario_id: str
    suite: str
    category: str
    variant: str
    answer: str
    citations: list[str]
    recall: Any
    expected: str
    evidence_ids: list[str]
    retrieval: dict[str, float]
    judge: dict[str, float]
    latency_ms: float
    memory_count: int
    operation_ok: bool | None = None
    error: str = ""
    metadata: dict[str, Any] = field(default_factory=dict)

    def json(self) -> dict[str, Any]:
        return asdict(self)
