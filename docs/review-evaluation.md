# Review-quality evaluation

The development harness compares three inputs for the same code changes:

| Strategy | Reviewer context |
| --- | --- |
| `packs` | Sonar's change-review pack response, including coverage and omissions |
| `diff` | Unified Git diff with three lines of context |
| `full` | The same diff plus all baseline and current source files |

Every trial uses the same review instruction and finding schema. Bug labels,
oracles, descriptions, and required-context annotations stay outside the reviewer
input. Correct controls are mixed with regressions, and trial order is shuffled
with a recorded seed. Repeats allow observing reviewer variation on the same
examples; they do not add independent bug cases.

The initial corpus has three synthetic Python regressions—pagination, mandatory
roles, and expiry units—and three corresponding correct changes. Each fixture
has an executable contract: its baseline and control must pass, while its buggy
version must fail an assertion. This corpus exercises the harness and exposes
context gaps; it does not establish production or cross-language review quality.

## Prepare an experiment

Python 3.8+, Git, and a built Sonar CLI are required. No Python packages, model
SDK, credentials, or network calls are needed for preparation and scoring.

```sh
cargo build --locked
python3 -B scripts/review_eval.py prepare \
  --output /tmp/sonar-review-experiment \
  --sonar target/debug/sonar \
  --max-files 6 --max-tokens 16000 --seed 42 --repeats 2
```

Preparation verifies fixture contracts and creates 36 trials (six cases × three
strategies × two repeats). Use a fresh output directory for each experiment.
The Sonar binary hash, corpus fingerprint, budgets, seed, snapshots, input
fingerprints, input bytes, and preparation timings are recorded. The experiment
manifest and private truth are fingerprinted too, so accidental changes to source
visibility or expected labels cannot silently alter scoring. Indexes and
baseline caches are isolated inside that directory; preparation invokes no
compiler resolver or LLM. Pack export must fit one page for these small fixtures;
larger fixtures need explicit pagination support before use.

The layout is:

- `manifest.json`: strategy assignments, input fingerprints, visible source ranges,
  and preparation timings.
- `inputs/`: blinded reviewer JSON inputs; these are the only files the reviewer
  should receive.
- `private/`: expected issues, source snapshots, contract results, fixture repos,
  and indexes. Keep these away from the reviewer.
- `responses/`: reviewer output, elapsed time, status, and experiment identity.

The private/input split prevents accidental answer leakage through the stdin
protocol. It is not a security sandbox: use a trusted reviewer command that does
not inspect private files or external repositories.

## Run a reviewer

Provide an adapter command that reads one JSON object from stdin and returns one
JSON object on stdout. The harness makes no assumption about model vendor or SDK.
The command runs without a shell, from an empty temporary working directory.
Use absolute executable/script paths. Configure the same model, settings, and
prompt version for all strategies, and record them in `--label`.

```sh
python3 -B scripts/review_eval.py run \
  --output /tmp/sonar-review-experiment \
  --reviewer-command 'python3 /absolute/path/to/reviewer_adapter.py' \
  --label 'model-version;temperature=0;adapter=v1' --timeout 120
```

The adapter should submit the input's `task` as its review instruction and
`context` plus `snapshots` as data. Start a fresh model conversation for each
trial, disable additional retrieval/tools, and do not give it private truth.
Actual model execution and its costs belong to the explicitly selected adapter.
Model name/settings are self-reported experimental configuration, not verified
by the harness. Do not edit the adapter or switch a model within an experiment.

Responses follow this shape:

```json
{
  "model": "actual-model-version-if-known",
  "findings": [
    {
      "id": "f1",
      "title": "Concrete regression",
      "explanation": "Explain the trigger, observed behavior, and violated contract.",
      "citations": [
        {
          "path": "paging.py",
          "snapshot": "current",
          "snapshot_id": "copy-the-current-snapshot-from-the-input",
          "range": [3, 3],
          "quote": "    return records[offset:offset + limit - 1]\n"
        }
      ]
    }
  ],
  "usage": {"input_tokens": 123, "output_tokens": 45}
}
```

`findings: []` is a valid completed review. `model` and `usage` are optional;
report usage only when the adapter knows actual provider/tokenizer counts.
Citations require exact whole source spans including their original line endings,
a valid snapshot ID, and lines actually exposed in that strategy's input.
For packs, source citations must come from `reading_path.excerpts`; a signature
alone does not expose every line in its declaration's range.
File existence or proximity to a known issue is insufficient. Findings need
nonempty titles/explanations and unique IDs. At most 100 findings are accepted.

Runs resume by skipping completed responses. Timeouts, invalid JSON, protocol
errors, and nonzero exits remain failures; `--retry-failed` retries failed command
attempts. A successful command with invalid citations remains an evidence failure
in scoring. Use a new experiment when changing adapters or model configuration.
Neither stderr errors nor malformed responses become empty successful reviews.

## Adjudicate and score

A human must assess whether each valid finding describes an annotated issue or a
false positive. Inspect the input, response, and `private/truth.json`. Add explicit
judgments with a reason:

```json
{
  "judgments": [
    {
      "trial_id": "copy-trial-id-from-manifest",
      "finding_id": "f1",
      "issue_id": "short-page",
      "reason": "Correctly explains the missing final record, including limit=1."
    },
    {
      "trial_id": "another-trial-id",
      "finding_id": "f2",
      "issue_id": null,
      "reason": "The alleged regression preserves the documented behavior."
    }
  ]
}
```

Issue IDs come from private truth and must never be supplied to the model. An
accepted finding must cite the annotated faulty current code as well as explain
the same defect. Human semantic judgment is still required: location overlap is
only a consistency check. Multiple findings about one issue count as duplicates,
not extra detections. If a reviewer discovers a plausible unannotated defect,
resolve the corpus annotation and version a new experiment rather than silently
calling it a true positive or false positive.

```sh
# First inspect citation validity and outstanding adjudications.
python3 -B scripts/review_eval.py score --output /tmp/sonar-review-experiment

# Then produce the adjudicated report.
python3 -B scripts/review_eval.py score \
  --output /tmp/sonar-review-experiment \
  --adjudications /absolute/path/to/judgments.json \
  --report /tmp/sonar-review-experiment/report.json
```

Per strategy, reports include:

- Grounded precision: distinct accepted issue detections divided by accepted
  detections, false positives, duplicates, and invalid-evidence findings.
- Recall and missed issues against the annotated regressions. Correct controls
  contribute false-positive measurements, not recall denominators.
- False-positive trial rate on correct controls, plus finding-level counts.
- Missing, failed, unadjudicated, and invalid-evidence counts. Missing/failed
  trials suppress quality scores; they are not interpreted as empty reviews.
- Reviewer elapsed time, measured prepared/attempted input bytes, and optional
  runner-reported input/output tokens with a usage-coverage count. Preparation
  timings are separate and shared across repeats. Bytes are not token counts or
  monetary cost; incomplete usage reporting must not be compared as full usage.
- Annotated required-context visibility, and missed issues occurring alongside
  missing required context. This co-occurrence does not prove that the absence
  caused the miss. Context annotations can also be incomplete.

Precision/recall remain unset until every trial in that strategy completes and
all valid findings are adjudicated. Precision is undefined when there are no
findings. Unknown judgments, stale input fingerprints, and mixed reviewer
identities are errors. Preserve raw responses and adjudication reasons so a
reported comparison is auditable.

The authorization and expiry fixtures need unchanged Python policy or settings
files. These originally exposed missing context in packs. The Python local-import
provider now links those files, and shared module-header ranges retain their
contract comments. The CLI integration test requires that packs expose the
annotated supporting spans at the default budget. A diff still omits the unchanged
files; the full-file strategy includes them. Smaller file/source budgets can still
omit supporting context, which the report measures. This verifies context delivery,
not an improvement in LLM bug detection before running an actual model comparison.

## Extend and test

Fixtures live in `tests/fixtures/review_eval/*.json`. The first format supports
edits to existing ordinary Python source paths; added/deleted/renamed files and
other language runtimes need explicit fixture/oracle and diff-visibility support.
Each group supplies baseline, buggy, and control file maps; an executable Python
oracle; one uniquely located issue annotation; and supporting-context spans.
Fixture oracles are trusted development code and execute locally.

```sh
python3 -B -m unittest discover -s scripts -p 'test_review_eval.py'
```

The CLI integration test runs when `target/debug/sonar` exists; build it first to
exercise preparation with real review packs. Other tests validate citation
provenance, diff visibility, semantic adjudication requirements, duplicate/false
positive accounting, missing context, timeouts, resumability, and stale inputs.
Scripted reviewers used by tests verify plumbing only; they are not LLM quality
results.
