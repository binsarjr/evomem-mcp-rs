from __future__ import annotations

import json
import shutil
from pathlib import Path

import httpx
import pytest
from evomem_eval.adapters import _memoryagentbench, internal_scenarios, load_suite
from evomem_eval.clients import EvomemProcess, decode_mcp_response, redact
from evomem_eval.metrics import retrieval_metrics, validate_judge
from evomem_eval.runner import _judge_json


def test_internal_covers_author_risks() -> None:
    assert {s.category for s in internal_scenarios()} == {
        "single-author parity",
        "cross-author retrieval",
        "provenance",
        "duplicates",
        "conflicts",
        "updates",
        "temporal",
        "abstention",
        "own forget",
        "blocked cross forget",
    }


def test_smoke_adapters_are_offline() -> None:
    for suite in ("longmemeval", "locomo", "memoryagentbench"):
        rows, metadata = load_suite(suite, "smoke", Path("unused"))
        assert rows and metadata["sha256"]


def test_metrics_judge_and_redaction() -> None:
    metric = retrieval_metrics(
        {"hits": [{"text": "evidence e1", "source_dir": "alice"}]},
        ("e1",),
        {"e1": "alice"},
    )
    assert (
        metric["all_at_5"] == 1
        and metric["noise_at_5"] == 0
        and metric["attribution_accuracy"] == 1
    )
    judge = validate_judge(
        {
            k: 90
            for k in (
                "correctness",
                "faithfulness",
                "completeness",
                "abstention",
                "provenance",
                "conflict_disclosure",
                "duplicate_free",
            )
        }
    )
    assert judge["overall"] == 90
    assert redact({"api_key": "secret", "nested": {"password": "secret"}}) == {
        "api_key": "[REDACTED]",
        "nested": {"password": "[REDACTED]"},
    }


def test_invalid_judge_fails() -> None:
    with pytest.raises(ValueError):
        validate_judge({"correctness": 101})


def test_fixture_is_valid_json() -> None:
    fixtures = Path(__file__).parents[1] / "fixtures"
    assert all(json.loads(path.read_text()) for path in fixtures.glob("*.json"))


def test_memoryagentbench_json_encoded_schema() -> None:
    rows = _memoryagentbench(
        [
            {
                "context": json.dumps("one long context"),
                "questions": json.dumps(["Where?"]),
                "answers": json.dumps([["There"]]),
                "metadata": json.dumps({"qa_pair_ids": ["qa-1"]}),
                "_split": "Accurate_Retrieval",
            }
        ]
    )
    assert (
        rows[0].id == "qa-1"
        and rows[0].answer == "There"
        and rows[0].category == "Accurate_Retrieval"
    )


def test_mcp_sse_parser_rejects_missing_data() -> None:
    response = httpx.Response(
        200,
        headers={"content-type": "text/event-stream"},
        text="event: message\n\n",
        request=httpx.Request("POST", "http://test"),
    )
    with pytest.raises(ValueError, match="no data"):
        decode_mcp_response(response)


def test_server_process_reports_early_exit(tmp_path: Path) -> None:
    with (
        pytest.raises(RuntimeError),
        EvomemProcess(Path(shutil.which("true") or "/usr/bin/true"), tmp_path),
    ):
        pass


def test_judge_schema_retries_once() -> None:
    class Judge:
        calls = 0

        def json(self, _system: str, _prompt: str) -> dict[str, int]:
            self.calls += 1
            keys = (
                "correctness",
                "faithfulness",
                "completeness",
                "abstention",
                "provenance",
                "conflict_disclosure",
                "duplicate_free",
            )
            return {key: 90 for key in keys} if self.calls == 2 else {"correctness": 90}

    judge = Judge()
    assert _judge_json(judge, "score this")["overall"] == 90  # type: ignore[arg-type]
    assert judge.calls == 2
