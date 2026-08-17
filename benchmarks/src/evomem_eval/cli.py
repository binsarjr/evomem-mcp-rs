from __future__ import annotations

import argparse
import json
import os
import re
from contextlib import nullcontext
from datetime import UTC, datetime
from pathlib import Path

from .adapters import load_suite
from .fake_server import FakeOpenAI
from .metrics import summarize
from .models import CaseResult
from .runner import run

SUITES = ("internal", "longmemeval", "locomo", "memoryagentbench")
BENCHMARKS_ROOT = Path(__file__).resolve().parents[2]


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="evomem-eval")
    sub = parser.add_subparsers(dest="command", required=True)
    execute = sub.add_parser(
        "run", help="run anonymous versus provenance A/B evaluation"
    )
    execute.add_argument("--suite", choices=("all",) + SUITES, default="all")
    execute.add_argument("--profile", choices=("smoke", "full"), default="smoke")
    execute.add_argument(
        "--lane", choices=("oracle-write", "agent-write"), default="oracle-write"
    )
    execute.add_argument("--server-bin", type=Path)
    execute.add_argument("--results", type=Path, default=BENCHMARKS_ROOT / "results")
    execute.add_argument("--cache", type=Path, default=BENCHMARKS_ROOT / ".cache")
    execute.add_argument(
        "--data-path", type=Path, help="local JSON/JSONL for one selected suite"
    )
    execute.add_argument("--run-id")
    execute.add_argument("--max-cases", type=int)
    execute.add_argument(
        "--fake-model",
        action="store_true",
        help="start a local fake OpenAI-compatible reader and judge",
    )
    execute.add_argument(
        "--endpoint",
        help="existing MCP endpoint; intentionally disabled unless --allow-remote-write",
    )
    execute.add_argument("--allow-remote-write", action="store_true")
    compare = sub.add_parser("compare", help="rebuild a summary from a run")
    compare.add_argument("run", type=Path)
    pilot = sub.add_parser(
        "pilot-check", help="check a read-only 14-day shared-brain pilot"
    )
    pilot.add_argument("brain", type=Path)
    pilot.add_argument(
        "--since", required=True, help="ISO date/time at least 14 days ago"
    )
    pilot.add_argument(
        "--checks",
        type=Path,
        help="JSON file with cross_author_checks and misattributions",
    )
    return parser


def _pilot(args: argparse.Namespace) -> int:
    since = datetime.fromisoformat(args.since.replace("Z", "+00:00"))
    if since.tzinfo is None:
        since = since.replace(tzinfo=UTC)
    counts: dict[str, int] = {}
    bodies: list[str] = []
    for path in args.brain.glob("*/*.md"):
        if datetime.fromtimestamp(path.stat().st_mtime, UTC) < since:
            continue
        counts[path.parent.name] = counts.get(path.parent.name, 0) + 1
        body = path.read_text(errors="replace").split("---", 2)[-1]
        bodies.append(re.sub(r"\s+", " ", body).strip().lower())
    checks = json.loads(args.checks.read_text()) if args.checks else {}
    authors = {k: v for k, v in counts.items() if k != "inbox"}
    duration = (datetime.now(UTC) - since).days
    exact_dup_rate = 1 - len(set(bodies)) / len(bodies) if bodies else 0
    gates = {
        "at least 14 days": duration >= 14,
        "two human authors": len(authors) >= 2,
        "each author has >=10 memories": len(authors) >= 2
        and all(v >= 10 for v in authors.values()),
        "no inbox writes": counts.get("inbox", 0) == 0,
        "no misattribution": checks.get("misattributions") == 0,
        "exact duplicates <=10%": exact_dup_rate <= 0.10,
        "five cross-author checks": len(checks.get("cross_author_checks", [])) >= 5,
    }
    print(
        json.dumps(
            {
                "authors": authors,
                "exact_duplicate_rate": exact_dup_rate,
                "gates": gates,
                "passed": all(gates.values()),
            },
            indent=2,
        )
    )
    return 0 if all(gates.values()) else 1


def main(argv: list[str] | None = None) -> None:
    args = _parser().parse_args(argv)
    if args.command == "pilot-check":
        raise SystemExit(_pilot(args))
    if args.command == "compare":
        path = args.run / "cases.jsonl" if args.run.is_dir() else args.run
        rows = [
            CaseResult(**json.loads(line))
            for line in path.read_text().splitlines()
            if line.strip()
        ]
        print(json.dumps(summarize(rows), indent=2))
        return
    if args.endpoint and not args.allow_remote_write:
        raise SystemExit(
            "refusing remote writes; use the default ephemeral server or explicitly pass --allow-remote-write"
        )
    project = Path(__file__).resolve().parents[3]
    binary = args.server_bin or project / "target" / "debug" / "evomem-mcp-rs"
    if not args.endpoint and not binary.is_file():
        raise SystemExit(
            f"server binary not found: {binary}; run `cargo build` in {project}"
        )
    names = SUITES if args.suite == "all" else (args.suite,)
    if args.data_path and args.suite == "all":
        raise SystemExit("--data-path requires one explicit --suite")
    scenarios, sources = [], {}
    for name in names:
        loaded, source = load_suite(name, args.profile, args.cache, args.data_path)
        scenarios.extend(loaded)
        sources[name] = source
    limit = args.max_cases or (5 if args.profile == "smoke" else None)
    if limit:
        kept, by_category = [], {}
        for scenario in scenarios:
            key = (scenario.suite, scenario.category)
            if by_category.get(key, 0) < limit:
                kept.append(scenario)
                by_category[key] = by_category.get(key, 0) + 1
        scenarios = kept
    run_id = args.run_id or datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    fake_context = FakeOpenAI() if args.fake_model else nullcontext()
    with fake_context as fake:
        if fake:
            os.environ.update(
                {
                    "EVOMEM_EVAL_WORKER_BASE_URL": fake.base_url,
                    "EVOMEM_EVAL_WORKER_MODEL": "fake",
                    "EVOMEM_EVAL_JUDGE_BASE_URL": fake.base_url,
                    "EVOMEM_EVAL_JUDGE_MODEL": "fake",
                }
            )
        summary = run(
            scenarios,
            args.results / run_id,
            binary,
            args.fake_model,
            args.lane,
            {
                "run_id": run_id,
                "profile": args.profile,
                "lane": args.lane,
                "sources": sources,
                "remote_endpoint": args.endpoint or "",
            },
            args.endpoint,
        )
    print(json.dumps(summary, indent=2))
    raise SystemExit(0 if summary.get("passed") else 1)
