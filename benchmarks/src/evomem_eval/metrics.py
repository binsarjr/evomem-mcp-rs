from __future__ import annotations

import json
import re
import statistics
from collections import defaultdict
from typing import Any

from .models import CaseResult

JUDGE_KEYS = (
    "correctness",
    "faithfulness",
    "completeness",
    "abstention",
    "provenance",
    "conflict_disclosure",
    "duplicate_free",
)
RETRIEVAL_KEYS = (
    "any_at_5",
    "all_at_5",
    "recall_at_5",
    "mrr",
    "noise_at_5",
    "attribution_coverage",
    "attribution_accuracy",
    "exact_duplicate_rate",
    "duplicate_amplification",
)


def retrieval_metrics(
    recall: Any,
    evidence_ids: tuple[str, ...],
    expected_authors: dict[str, str] | None = None,
) -> dict[str, float]:
    hits = recall.get("hits", []) if isinstance(recall, dict) else []
    top_hits = hits[:5]
    found = [
        e
        for e in evidence_ids
        if any(e.lower() in json.dumps(hit).lower() for hit in top_hits)
    ]
    ranks = [
        rank
        for rank, hit in enumerate(hits, 1)
        if any(e.lower() in json.dumps(hit).lower() for e in evidence_ids)
    ]
    attributed = [
        evidence
        for evidence in evidence_ids
        if any(
            evidence.lower() in json.dumps(hit).lower() and hit.get("source_dir")
            for hit in top_hits
        )
    ]
    correct_authors = [
        evidence
        for evidence in attributed
        if any(
            evidence.lower() in json.dumps(hit).lower()
            and str(hit.get("source_dir", "")).lower()
            == str((expected_authors or {}).get(evidence, "")).lower()
            for hit in top_hits
        )
    ]
    slugs = [str(hit.get("slug", "")) for hit in top_hits if hit.get("slug")]
    snippets = [
        re.sub(r"\[evidence:[^]]+\]|\[at:[^]]*\]", "", str(hit.get("snippet", "")))
        .strip()
        .lower()
        for hit in top_hits
        if hit.get("snippet")
    ]
    return {
        "any_at_5": float(not evidence_ids or bool(found)),
        "all_at_5": float(len(found) == len(evidence_ids)),
        "recall_at_5": len(found) / len(evidence_ids) if evidence_ids else 1.0,
        "mrr": 1.0 / min(ranks) if ranks else (1.0 if not evidence_ids else 0.0),
        "noise_at_5": sum(
            not any(e.lower() in json.dumps(hit).lower() for e in evidence_ids)
            for hit in top_hits
        )
        / max(1, len(top_hits))
        if evidence_ids
        else float(bool(top_hits)),
        "attribution_coverage": len(attributed) / len(evidence_ids)
        if evidence_ids
        else 1.0,
        "attribution_accuracy": len(correct_authors) / len(attributed)
        if attributed
        else 1.0,
        "exact_duplicate_rate": 1 - len(set(snippets)) / len(snippets)
        if snippets
        else 0.0,
        "duplicate_amplification": 1 - len(set(slugs)) / len(slugs) if slugs else 0.0,
    }


def validate_judge(value: dict[str, Any]) -> dict[str, float]:
    missing = set(JUDGE_KEYS) - value.keys()
    if missing:
        raise ValueError(f"judge missing fields: {sorted(missing)}")
    parsed = {k: float(value[k]) for k in JUDGE_KEYS}
    if any(v < 0 or v > 100 for v in parsed.values()):
        raise ValueError("judge scores must be between 0 and 100")
    parsed["overall"] = statistics.fmean(parsed.values())
    return parsed


def summarize(cases: list[CaseResult]) -> dict[str, Any]:
    variants: dict[str, list[CaseResult]] = defaultdict(list)
    for case in cases:
        variants[case.variant].append(case)
    summary: dict[str, Any] = {"variants": {}, "gates": []}
    for name, rows in variants.items():
        summary["variants"][name] = {
            **{
                k: statistics.fmean(r.retrieval.get(k, 0) for r in rows)
                for k in RETRIEVAL_KEYS
            },
            **{
                f"judge_{k}": statistics.fmean(r.judge[k] for r in rows)
                for k in JUDGE_KEYS + ("overall",)
            },
            "latency_p50_ms": statistics.median(r.latency_ms for r in rows),
            "latency_p95_ms": sorted(r.latency_ms for r in rows)[
                max(0, int(len(rows) * 0.95) - 1)
            ],
            "operation_success": statistics.fmean(
                float(r.operation_ok) for r in rows if r.operation_ok is not None
            )
            if any(r.operation_ok is not None for r in rows)
            else 1.0,
            "memory_count": sum(r.memory_count for r in rows),
            "stale_claim_rate": statistics.fmean(
                r.retrieval["noise_at_5"]
                for r in rows
                if r.category in {"updates", "temporal"}
            )
            if any(r.category in {"updates", "temporal"} for r in rows)
            else 0.0,
        }
    if {"anonymous", "provenance"} <= summary["variants"].keys():
        a, p = summary["variants"]["anonymous"], summary["variants"]["provenance"]
        cross = [
            r for r in variants["provenance"] if r.category == "cross-author retrieval"
        ]
        conflicts = [r for r in variants["provenance"] if r.category == "conflicts"]
        external_suites = {
            r.suite for r in variants["provenance"] if r.suite != "internal"
        }
        external_drop = max(
            (
                statistics.fmean(
                    r.judge["overall"]
                    for r in variants["anonymous"]
                    if r.suite == suite
                )
                - statistics.fmean(
                    r.judge["overall"]
                    for r in variants["provenance"]
                    if r.suite == suite
                )
                for suite in external_suites
            ),
            default=0,
        )
        checks = {
            "provenance judge >= 90": p["judge_provenance"] >= 90,
            "forget isolation = 100%": p["operation_success"] == 1,
            "answer quality delta >= -2": p["judge_overall"] >= a["judge_overall"] - 2,
            "retrieval delta >= -2pp": p["any_at_5"] >= a["any_at_5"] - 0.02,
            "noise@5 delta <= 2pp": p["noise_at_5"] <= a["noise_at_5"] + 0.02,
            "attribution accuracy = 100%": p["attribution_accuracy"] == 1,
            "duplicate amplification = 0": p["duplicate_amplification"] == 0,
            "external category drop <= 3": external_drop <= 3,
            "memory counts identical": p["memory_count"] == a["memory_count"],
            "p95 latency <= 1.10x": p["latency_p95_ms"] <= a["latency_p95_ms"] * 1.10,
        }
        if cross:
            checks["cross-author Any@5 >= 95%"] = (
                statistics.fmean(r.retrieval["any_at_5"] for r in cross) >= 0.95
            )
        if conflicts:
            checks["conflict completeness >= 95%"] = (
                statistics.fmean(r.retrieval["all_at_5"] for r in conflicts) >= 0.95
            )
            checks["silent conflict resolution = 0"] = all(
                r.judge["conflict_disclosure"] >= 90 for r in conflicts
            )
        summary["gates"] = [{"name": k, "passed": v} for k, v in checks.items()]
        summary["passed"] = all(checks.values())
    return summary
