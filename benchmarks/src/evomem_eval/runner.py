from __future__ import annotations

import hashlib
import json
import os
import re
import time
from contextlib import ExitStack
from pathlib import Path
from typing import Any

from .clients import ChatClient, EvomemProcess, MCPClient, redact, temporary_root
from .metrics import retrieval_metrics, summarize, validate_judge
from .models import CaseResult, Event, Scenario


def _prompt_expected(s: Scenario, fake: bool) -> str:
    return (
        f"\nFAKE_EXPECTED_JSON:{json.dumps({'answer': s.answer, 'evidence_ids': s.evidence_ids})}"
        if fake
        else ""
    )


def _model_json(client: ChatClient, system: str, prompt: str) -> dict[str, Any]:
    try:
        return client.json(system, prompt)
    except (ValueError, KeyError, json.JSONDecodeError):
        return client.json(system + " Return one valid JSON object only.", prompt)


def _judge_json(client: ChatClient, prompt: str) -> dict[str, float]:
    try:
        return validate_judge(
            _model_json(client, "You are a strict memory QA judge.", prompt)
        )
    except ValueError:
        return validate_judge(
            client.json(
                "You are a strict memory QA judge. Return all seven required numeric fields in one JSON object.",
                prompt,
            )
        )


def _manifest(
    scenario: Scenario, lane: str, worker: ChatClient, fake: bool
) -> tuple[Event, ...]:
    if lane == "oracle-write":
        return scenario.events
    extracted = []
    for event in scenario.events:
        marker = f"\nFAKE_EVENT_JSON:{json.dumps({'text': event.text})}" if fake else ""
        value = _model_json(
            worker,
            "You are a durable memory writer. Return JSON with a memories string array; omit transient details and never invent facts.",
            f"EVENT:\n{event.text}{marker}",
        )
        memories = value.get("memories", [])
        text = "\n".join(str(item) for item in memories if str(item).strip())
        if text:
            extracted.append(Event(event.id, event.author, text, event.timestamp))
    return tuple(extracted)


def _remember_all(
    client: MCPClient, scenario: Scenario, events: tuple[Event, ...], provenance: bool
) -> list[tuple[str, str]]:
    slugs = []
    for event in events:
        author = event.author if provenance else None
        text = f"[evidence:{event.id}] [at:{event.timestamp}] {event.text}"
        result = client.call(
            "memory_remember",
            {
                "text": text,
                "title": f"{scenario.id}-{event.id}",
                "tags": ["evaluation"],
            },
            author,
        )
        slugs.append((event.author, str(result["slug"])))
    return slugs


def _case(
    client: MCPClient,
    scenario: Scenario,
    events: tuple[Event, ...],
    slugs: list[tuple[str, str]],
    write_count: int,
    variant: str,
    reader: ChatClient,
    judge: ChatClient,
    fake: bool,
) -> CaseResult:
    provenance = variant == "provenance"
    operation_ok: bool | None = None
    if scenario.operation == "own_forget":
        client.call(
            "memory_forget", {"slug": slugs[0][1]}, slugs[0][0] if provenance else None
        )
        operation_ok = True
    elif scenario.operation == "cross_forget":
        try:
            client.call(
                "memory_forget", {"slug": slugs[0][1]}, "bob" if provenance else None
            )
            operation_ok = not provenance
        except RuntimeError:
            operation_ok = provenance
    started = time.perf_counter()
    recall = client.call(
        "memory_recall",
        {"query": scenario.question, "mode": "search"},
        "reader" if provenance else None,
    )
    latency = (time.perf_counter() - started) * 1000
    answer = _model_json(
        reader,
        "Answer only from recalled memory. Return JSON: answer string and citations array.",
        f"QUESTION:\n{scenario.question}\nRECALLED MEMORY:\n{json.dumps(recall)}{_prompt_expected(scenario, fake)}",
    )
    judge_prompt = f"REFERENCE: {scenario.answer}\nCANDIDATE: {answer.get('answer', '')}\nRECALL: {json.dumps(recall)}\nScore 0-100 JSON fields correctness, faithfulness, completeness, abstention, provenance, conflict_disclosure, duplicate_free.{_prompt_expected(scenario, fake)}"
    judged = _judge_json(judge, judge_prompt)
    return CaseResult(
        scenario_id=scenario.id,
        suite=scenario.suite,
        category=scenario.category,
        variant=variant,
        answer=str(answer.get("answer", "")),
        citations=list(map(str, answer.get("citations", []))),
        recall=recall,
        expected=scenario.answer,
        evidence_ids=list(scenario.evidence_ids),
        retrieval=retrieval_metrics(
            recall, scenario.evidence_ids, {event.id: event.author for event in events}
        ),
        judge=judged,
        latency_ms=latency,
        memory_count=write_count,
        operation_ok=operation_ok,
        metadata={"expected_authors": {event.id: event.author for event in events}},
    )


def _pairwise(rows: list[CaseResult], judge: ChatClient) -> None:
    grouped: dict[str, dict[str, CaseResult]] = {}
    for row in rows:
        grouped.setdefault(row.scenario_id, {})[row.variant] = row
    for pair in grouped.values():
        if set(pair) != {"anonymous", "provenance"}:
            continue
        anonymous, provenance = pair["anonymous"], pair["provenance"]
        if abs(anonymous.judge["overall"] - provenance.judge["overall"]) > 2:
            continue
        decisions = []
        for first, second, labels in (
            (anonymous, provenance, ("anonymous", "provenance")),
            (provenance, anonymous, ("provenance", "anonymous")),
        ):
            value = _model_json(
                judge,
                "You are a strict pairwise memory answer judge. Return JSON winner (A, B, or tie) and reason.",
                f"A ANSWER: {first.answer}\nA CITATIONS: {first.citations}\nA RECALL: {json.dumps(first.recall)}\nB ANSWER: {second.answer}\nB CITATIONS: {second.citations}\nB RECALL: {json.dumps(second.recall)}\nREFERENCE: {first.expected}",
            )
            winner = str(value.get("winner", "tie")).lower()
            decisions.append(
                labels[0] if winner == "a" else labels[1] if winner == "b" else "tie"
            )
        decision = decisions[0] if decisions[0] == decisions[1] else "tie"
        anonymous.metadata["pairwise"] = provenance.metadata["pairwise"] = {
            "decision": decision,
            "position_swapped": True,
        }


def run(
    scenarios: list[Scenario],
    result_dir: Path,
    binary: Path,
    fake: bool,
    lane: str,
    metadata: dict[str, Any],
    endpoint: str | None = None,
) -> dict[str, Any]:
    result_dir.mkdir(parents=True, exist_ok=True)
    completed: dict[tuple[str, str], CaseResult] = {}
    case_path = result_dir / "cases.jsonl"
    if case_path.exists():
        for line in case_path.read_text().splitlines():
            raw = json.loads(line)
            completed[(raw["scenario_id"], raw["variant"])] = CaseResult(**raw)
    manifests: dict[str, list[dict[str, str]]] = {}
    manifest_cache: dict[tuple[str, tuple[str, ...]], tuple[Event, ...]] = {}
    with temporary_root() as temp, ExitStack() as stack:
        urls = (
            {variant: endpoint for variant in ("anonymous", "provenance")}
            if endpoint
            else {
                variant: stack.enter_context(
                    EvomemProcess(binary, Path(temp) / variant)
                ).url
                for variant in ("anonymous", "provenance")
            }
        )
        run_id = re.sub(r"[^a-z0-9_-]", "-", str(metadata["run_id"]).lower())
        clients: dict[tuple[str, str], MCPClient] = {}
        slugs_by_manifest: dict[
            tuple[str, str, tuple[str, ...]], list[tuple[str, str]]
        ] = {}

        def client_for(variant: str, memory_id: str) -> MCPClient:
            key = (variant, memory_id)
            if key not in clients:
                digest = hashlib.sha256(memory_id.encode()).hexdigest()[:12]
                client = MCPClient(urls[variant], f"eval-{run_id}-{variant}-{digest}")
                client.initialize()
                stack.callback(client.close)
                clients[key] = client
            return clients[key]

        reader, judge = ChatClient("WORKER"), ChatClient("JUDGE")
        for scenario in scenarios:
            memory_id = scenario.memory_id or scenario.id
            event_ids = tuple(event.id for event in scenario.events)
            manifest_key = (memory_id, event_ids)
            if manifest_key not in manifest_cache:
                manifest_cache[manifest_key] = _manifest(scenario, lane, reader, fake)
                known = {event["id"] for event in manifests.get(memory_id, [])}
                manifests.setdefault(memory_id, []).extend(
                    {
                        "id": event.id,
                        "author": event.author,
                        "timestamp": event.timestamp,
                        "text": event.text,
                    }
                    for event in manifest_cache[manifest_key]
                    if event.id not in known
                )
            events = manifest_cache[manifest_key]
            for variant in ("anonymous", "provenance"):
                if (scenario.id, variant) in completed:
                    continue
                client = client_for(variant, memory_id)
                write_key = (variant, memory_id, event_ids)
                first_write = write_key not in slugs_by_manifest
                try:
                    if first_write:
                        slugs_by_manifest[write_key] = _remember_all(
                            client, scenario, events, variant == "provenance"
                        )
                    result = _case(
                        client,
                        scenario,
                        events,
                        slugs_by_manifest[write_key],
                        len(events) if first_write else 0,
                        variant,
                        reader,
                        judge,
                        fake,
                    )
                except Exception as exc:  # noqa: BLE001 - one failed benchmark case must not discard the run
                    result = CaseResult(
                        scenario.id,
                        scenario.suite,
                        scenario.category,
                        variant,
                        "",
                        [],
                        {},
                        scenario.answer,
                        list(scenario.evidence_ids),
                        {
                            "any_at_5": 0,
                            "all_at_5": 0,
                            "recall_at_5": 0,
                            "mrr": 0,
                            "noise_at_5": 1,
                            "attribution_coverage": 0,
                            "attribution_accuracy": 0,
                            "exact_duplicate_rate": 0,
                            "duplicate_amplification": 0,
                        },
                        {
                            k: 0
                            for k in (
                                "correctness",
                                "faithfulness",
                                "completeness",
                                "abstention",
                                "provenance",
                                "conflict_disclosure",
                                "duplicate_free",
                                "overall",
                            )
                        },
                        0,
                        len(events) if first_write else 0,
                        error=str(exc),
                    )
                completed[(scenario.id, variant)] = result
                with case_path.open("a") as handle:
                    handle.write(json.dumps(result.json(), sort_keys=True) + "\n")
    rows = list(completed.values())
    _pairwise(rows, judge)
    case_path.write_text(
        "".join(json.dumps(row.json(), sort_keys=True) + "\n" for row in rows)
    )
    (result_dir / "manifests.json").write_text(
        json.dumps(manifests, indent=2, sort_keys=True) + "\n"
    )
    summary = summarize(rows)
    (result_dir / "summary.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n"
    )
    (result_dir / "run.json").write_text(
        json.dumps(
            redact(
                metadata
                | {
                    "environment": {
                        k: v
                        for k, v in os.environ.items()
                        if k.startswith("EVOMEM_EVAL_")
                    }
                }
            ),
            indent=2,
            sort_keys=True,
        )
        + "\n"
    )
    report = [
        "# Evomem author evaluation",
        "",
        f"Overall gate: **{'PASS' if summary.get('passed') else 'FAIL'}**",
        "",
        "## Gates",
        "",
    ]
    report += [
        f"- [{'x' if gate['passed'] else ' '}] {gate['name']}"
        for gate in summary.get("gates", [])
    ]
    (result_dir / "summary.md").write_text("\n".join(report) + "\n")
    return summary
