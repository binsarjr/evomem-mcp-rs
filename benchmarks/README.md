# evomem-eval

`evomem-eval` answers one narrow question: does per-human author provenance
improve a shared namespace without making recall noisier? It is a separate
Python process. It does not alter the Rust server, storage schema, or deployed
brains.

## What the A/B test means

Every case creates one immutable write manifest, then replays it into two fresh
ephemeral brains:

- `anonymous`: every write uses the existing `inbox` fallback;
- `provenance`: the same write includes `X-Evomem-Author: <human>`.

Recall remains namespace-global in both variants. Harness names are never used
as authors. Conflicting claims must remain visible with their source; equal
facts should be answered once without erasing their multiple sources.

The internal suite covers parity, cross-author recall, attribution, duplicates,
conflicts, updates, temporal questions, abstention, own-author forget, and
blocked cross-author forget. The external adapters cover LongMemEval, LoCoMo,
and MemoryAgentBench.

## Step 1: prerequisites

Install `uv`, Python 3.11+, and build the server once from the repository root:

```bash
cargo build
uv sync --project benchmarks
```

The default runner starts two loopback-only server processes with temporary,
separate roots. It cleans them up after the run. It will not write to an
existing endpoint; `--endpoint` is rejected unless remote writes are explicitly
acknowledged with `--allow-remote-write`. An acknowledged remote run creates
fresh `eval-<run-id>-anonymous` and `eval-<run-id>-provenance` namespaces.

## Step 2: smoke-test the entire pipeline

This uses tiny committed fixtures and a local fake OpenAI-compatible HTTP
server. It needs no network, API key, or paid model.

```bash
uv run --project benchmarks evomem-eval run \
  --suite all \
  --profile smoke \
  --fake-model
```

Results are written to `benchmarks/results/<run-id>/` when run from this
directory, or to the relative `results/` directory of the current shell:

- `run.json`: redacted configuration and pinned source metadata;
- `manifests.json`: the immutable writes replayed into both variants;
- `cases.jsonl`: resumable per-case results;
- `summary.json`: metrics and machine-readable gates;
- `summary.md`: short gate report.

Resume a partial run with `--run-id <existing-id>`. Recalculate its summary:

```bash
uv run --project benchmarks evomem-eval compare results/<run-id>
```

## Step 3: configure real reader and judge models

Worker/reader and judge use separate OpenAI-compatible configurations. Keep
keys in your shell or `benchmarks/.env`; `.env` is ignored and values whose
names look secret are redacted from run metadata.

```bash
export EVOMEM_EVAL_WORKER_BASE_URL=https://reader.example/v1
export EVOMEM_EVAL_WORKER_MODEL=reader-model
export EVOMEM_EVAL_WORKER_API_KEY=...

export EVOMEM_EVAL_JUDGE_BASE_URL=https://judge.example/v1
export EVOMEM_EVAL_JUDGE_MODEL=judge-model
export EVOMEM_EVAL_JUDGE_API_KEY=...
```

Both clients request temperature `0`, seed `42`, and JSON output. Invalid judge
JSON is retried once and then fails the case. The judge is mandatory; there is
no lexical-score fallback that can silently turn model failure into a pass.

## Step 4: run official datasets

```bash
uv run --project benchmarks evomem-eval run --suite all --profile full
```

The full profile downloads official LongMemEval and LoCoMo JSON into
`benchmarks/.cache/`, records the SHA-256 in `run.json`, and loads
`ai-hyz/MemoryAgentBench` through Hugging Face `datasets`. All three sources use
reviewed commit revisions pinned in `src/evomem_eval/adapters.py`.

Use `--max-cases N` for a budgeted per-category trial. `--lane oracle-write` remembers the
annotated evidence directly. `--lane agent-write` first asks the worker model
to extract durable memories. Extraction happens once per event; that immutable
manifest is then replayed into both A/B variants so author headers are the only
difference.

For a reviewed local export, select exactly one suite and pass
`--data-path path/to/cases.json` (JSON or JSONL in the normalized fixture
shape). Its SHA-256 is recorded in `run.json`.

Remote endpoints behind a proxy can use
`EVOMEM_EVAL_MCP_AUTHORIZATION` (for example, `Bearer ...`). It is redacted
from `run.json`. Case and manifest files intentionally contain benchmark memory
text, so treat the result directory according to the source data's sensitivity.

## Metrics and author gate

Each variant reports Any@5, All@5, Recall@5, MRR, noise@5, strict judge dimensions,
operation isolation, memory count, and p50/p95 recall latency. The automatic
gate requires:

- provenance Any@5 at least 95%;
- provenance judge at least 90/100;
- own/cross-author forget checks at 100%;
- judge overall no more than 2 points below anonymous;
- Any@5 no more than 2 percentage points below anonymous;
- identical write counts;
- provenance p95 no more than 1.10x anonymous.

The synthetic suite is a regression gate, not proof of real-team usefulness.
Keep author provenance only after the pilot below and a reviewed full benchmark.
Failures shared by both variants indicate a general memory/benchmark issue, not
evidence to remove author provenance.

## Step 5: run the 14-day human pilot

Use a fresh namespace. Configure exactly human/team identities in
`X-Evomem-Author`; do not use `codex`, `claude`, or other harness names. Allow
all clients to recall the whole namespace. Do not migrate existing production
brains for this pilot.

After at least 14 days, export or mount the brain read-only and prepare:

```json
{
  "misattributions": 0,
  "cross_author_checks": [
    "check-1", "check-2", "check-3", "check-4", "check-5"
  ]
}
```

Then run:

```bash
uv run --project benchmarks evomem-eval pilot-check /read-only/pilot-brain \
  --since 2026-08-01 \
  --checks pilot-checks.json
```

The pilot passes only with two or more human authors, at least 10 durable
memories per author, no `inbox` writes, zero documented misattributions, exact
duplicates at or below 10%, and five documented cross-author recall checks.
The command reads files and metadata only; it never calls `memory_forget` or
writes to the brain.

## Development checks

```bash
uv run --project benchmarks pytest
cargo test
cargo clippy --all-targets -- -D warnings
git diff --check
```

Public benchmark references:

- [Evonic evaluator](https://github.com/anvie/evonic/tree/dev/evaluator) and [two-pass evaluation](https://github.com/anvie/evonic/blob/dev/docs/two-pass-evaluation.md)
- [LongMemEval](https://github.com/xiaowu0162/LongMemEval)
- [LoCoMo](https://github.com/snap-research/locomo)
- [MemoryAgentBench](https://github.com/HUST-AI-HYZ/MemoryAgentBench)
