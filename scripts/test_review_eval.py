"""Validate experiment isolation, evidence checks, and honest review metrics."""
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import shlex
import shutil
import sys
import tempfile
import unittest

import review_eval as evaluation


class ReviewEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.case = {"sources": {"current": {"a.py": "def value():\n    return 2\n"},
                                 "baseline": {"a.py": "def value():\n    return 1\n"}},
                     "snapshots": {"current": "new", "baseline": "old"}}
        self.trial = {"visible": [{"path": "a.py", "snapshot": "current", "range": [2, 2]}]}
        self.finding = {"id": "f1", "title": "A regression", "explanation": "Concrete evidence and effect.",
                        "citations": [{"path": "a.py", "snapshot": "current", "snapshot_id": "new",
                                       "range": [2, 2], "quote": "    return 2\n"}]}

    def validate(self, finding):
        return evaluation.validate_findings({"findings": [finding]}, self.case, self.trial)[0]

    def test_quotes_must_match_exact_delivered_source_and_snapshot(self):
        self.assertTrue(self.validate(self.finding)["valid"])
        for key, value in [("quote", "return 2"), ("snapshot_id", "stale"),
                           ("path", "../a.py"), ("range", [1, 2]),
                           ("range", [True, 2]), ("snapshot", "invented")]:
            with self.subTest(key=key, value=value):
                finding = copy.deepcopy(self.finding)
                finding["citations"][0][key] = value
                self.assertFalse(self.validate(finding)["valid"])

    def test_unicode_crlf_and_annotation_endings_keep_exact_coordinates(self):
        self.case["sources"]["current"]["a.py"] = "def value():\r\n    return '雪'\r\n"
        finding = copy.deepcopy(self.finding)
        finding["citations"][0]["quote"] = "    return '雪'\r\n"
        self.assertTrue(self.validate(finding)["valid"])
        finding["citations"][0]["quote"] = "    return '雪'\n"
        self.assertFalse(self.validate(finding)["valid"])
        annotation = evaluation.span({"a.py": "one\ntwo\n"}, {"path": "a.py", "needle": "two\n"})
        self.assertEqual(annotation["range"], [2, 2])

    def test_duplicate_ids_and_missing_findings_are_protocol_errors(self):
        for response in [None, {}, {"findings": "none"}, {"findings": [self.finding, self.finding]}]:
            with self.assertRaises(ValueError):
                evaluation.validate_findings(response, self.case, self.trial)

    def test_diff_visibility_tracks_both_sides_without_exposing_unchanged_files(self):
        patch = ("diff --git a/a.py b/a.py\n--- a/a.py\n+++ b/a.py\n@@ -1,2 +1,2 @@\n def value():\n"
                 "-    return 1\n+    return 2\n"
                 "diff --git a/b.py b/b.py\n--- a/b.py\n+++ b/b.py\n@@ -10 +10 @@\n-old\n+new\n")
        visible = evaluation.diff_visibility(patch)
        for snapshot in ("current", "baseline"):
            self.assertTrue(evaluation.shown(visible, {"path": "a.py", "snapshot": snapshot, "range": [1, 2]}))
            self.assertTrue(evaluation.shown(visible, {"path": "b.py", "snapshot": snapshot, "range": [10, 10]}))
            self.assertFalse(evaluation.shown(visible, {"path": "a.py", "snapshot": snapshot, "range": [1, 3]}))
            self.assertFalse(evaluation.shown(visible, {"path": "settings.py", "snapshot": snapshot, "range": [1, 1]}))

    def test_fixture_contracts_are_executable_and_reject_broken_controls(self):
        for fixture in sorted(evaluation.FIXTURES.glob("*.json")):
            with self.subTest(fixture=fixture):
                group = evaluation.read_json(fixture)
                self.assertEqual(evaluation.verify_fixture(group)["buggy"], "expected_assertion_failure")
        group["control"] = group["buggy"]
        with self.assertRaises(ValueError):
            evaluation.verify_fixture(group)

    def test_fixture_paths_cannot_escape_the_temporary_repository(self):
        with tempfile.TemporaryDirectory() as directory:
            for name in ("../escape", "/absolute", ".git/config", "a/../escape"):
                with self.assertRaises(ValueError):
                    evaluation.safe_files(Path(directory), {name: "content"})


class ReviewScoringTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.output = Path(temp.name)
        self.issue = {"id": "bug", "path": "a.py", "snapshot": "current", "range": [2, 2],
                      "required_context": [{"path": "settings.py", "snapshot": "current", "range": [1, 1]}]}
        self.truth = {"bug-case": {"issues": [self.issue], "snapshots": {"current": "new", "baseline": "old"},
                      "sources": {"current": {"a.py": "def value():\n    return 2\n", "settings.py": "EXPECTED = 1\n"},
                                  "baseline": {"a.py": "def value():\n    return 1\n"}}},
                      "control-case": {"issues": [], "snapshots": {"current": "new", "baseline": "old"},
                      "sources": {"current": {"a.py": "def value():\n    return 1\n"}, "baseline": {}}}}
        self.trials = []
        for variant in evaluation.VARIANTS:
            for case in self.truth:
                trial_id = variant + "-" + case
                value = {"input": trial_id}
                evaluation.write_json(self.output / "inputs" / (trial_id + ".json"), value)
                visible = [{"path": "a.py", "snapshot": "current", "range": [1, 2]}]
                if variant == "full" and case == "bug-case":
                    visible.append({"path": "settings.py", "snapshot": "current", "range": [1, 1]})
                trial = {"id": trial_id, "case": case, "variant": variant, "visible": visible,
                         "input": "inputs/" + trial_id + ".json", "input_digest": evaluation.digest(value), "input_utf8_bytes": 10}
                self.trials.append(trial)
                self.record(trial_id, [])
        manifest = {"trials": self.trials, "truth_digest": evaluation.digest(self.truth)}
        manifest["fingerprint"] = evaluation.digest(manifest)
        evaluation.write_json(self.output / "manifest.json", manifest)
        evaluation.write_json(self.output / "private/truth.json", self.truth)
        self.finding = {"id": "f1", "title": "Wrong return", "explanation": "It returns 2 instead of the required 1.",
                        "citations": [{"path": "a.py", "snapshot": "current", "snapshot_id": "new", "range": [2, 2], "quote": "    return 2\n"}]}

    def record(self, trial_id, findings, **updates):
        value = {"input": trial_id}
        record = {"trial_id": trial_id, "reviewer_label": "scripted-test", "command_digest": "test-only",
                  "input_digest": evaluation.digest(value), "status": "ok", "seconds": 0.5,
                  "response": {"findings": findings}}
        record.update(updates)
        evaluation.write_json(self.output / "responses" / (trial_id + ".json"), record)

    def judge(self, entries):
        path = self.output / "judgments.json"
        evaluation.write_json(path, {"judgments": [dict(trial_id=t, finding_id=f, issue_id=i, reason="Human verified the described failure.") for t, f, i in entries]})
        return path

    def test_unadjudicated_location_match_is_not_a_true_positive(self):
        self.record("packs-bug-case", [self.finding])
        report = evaluation.score(self.output)
        packs = report["metrics"]["packs"]
        self.assertEqual(packs["true_positives"], 0)
        self.assertEqual(packs["unadjudicated"], 1)
        self.assertIsNone(packs["grounded_precision"])
        self.assertIsNone(packs["recall"])
        self.assertIsNone(next(t for t in report["trials"] if t["trial_id"] == "packs-bug-case")["missed_issues"])

    def test_duplicates_and_false_positives_are_penalized_after_adjudication(self):
        duplicate = dict(self.finding, id="f2")
        self.record("packs-bug-case", [self.finding, duplicate])
        control = copy.deepcopy(self.finding)
        control["citations"][0]["quote"] = "    return 1\n"
        self.record("packs-control-case", [control])
        judgments = self.judge([("packs-bug-case", "f1", "bug"), ("packs-bug-case", "f2", "bug"), ("packs-control-case", "f1", None)])
        packs = evaluation.score(self.output, judgments)["metrics"]["packs"]
        self.assertEqual(packs["true_positives"], 1)
        self.assertEqual(packs["duplicates"], 1)
        self.assertEqual(packs["false_positives"], 1)
        self.assertAlmostEqual(packs["grounded_precision"], 1 / 3)
        self.assertEqual(packs["recall"], 1)
        self.assertEqual(packs["clean_trial_false_positive_rate"], 1)
        self.assertEqual(packs["missed_with_missing_context"], 0)

    def test_undelivered_evidence_is_rejected_and_context_absence_is_reported(self):
        invalid = copy.deepcopy(self.finding)
        invalid["citations"][0].update(path="settings.py", range=[1, 1], quote="EXPECTED = 1\n")
        self.record("packs-bug-case", [invalid])
        report = evaluation.score(self.output)
        self.assertEqual(report["metrics"]["packs"]["invalid_findings"], 1)
        self.assertEqual(report["metrics"]["packs"]["grounded_precision"], 0)
        self.assertEqual(report["metrics"]["packs"]["missing_context_issues"], 1)
        self.assertEqual(report["metrics"]["full"]["missing_context_issues"], 0)

    def test_missing_and_failed_trials_do_not_become_successful_empty_reviews(self):
        (self.output / "responses/packs-control-case.json").unlink()
        self.record("diff-control-case", [], status="error", error="timed out")
        report = evaluation.score(self.output)
        self.assertEqual(report["metrics"]["packs"]["missing"], 1)
        self.assertEqual(report["metrics"]["diff"]["failed"], 1)
        self.assertIsNone(report["metrics"]["packs"]["recall"])
        self.assertIsNone(report["metrics"]["diff"]["recall"])
        self.assertEqual(report["metrics"]["full"]["recall"], 0)
        self.assertEqual(report["metrics"]["full"]["token_usage_trials"], 0)

    def test_stale_input_and_unknown_adjudications_are_errors(self):
        path = self.judge([("unknown-trial", "f1", None)])
        with self.assertRaises(ValueError):
            evaluation.score(self.output, path)
        evaluation.write_json(self.output / "inputs/packs-bug-case.json", {"changed": True})
        with self.assertRaisesRegex(ValueError, "fingerprint mismatch"):
            evaluation.score(self.output)

    def test_changed_visibility_or_truth_cannot_silently_change_scores(self):
        manifest = evaluation.read_json(self.output / "manifest.json")
        manifest["trials"][0]["visible"].append({"path": "settings.py", "snapshot": "current", "range": [1, 1]})
        evaluation.write_json(self.output / "manifest.json", manifest)
        with self.assertRaisesRegex(ValueError, "manifest fingerprint mismatch"):
            evaluation.score(self.output)
        manifest["trials"][0]["visible"].pop()
        evaluation.write_json(self.output / "manifest.json", manifest)
        self.truth["bug-case"]["issues"] = []
        evaluation.write_json(self.output / "private/truth.json", self.truth)
        with self.assertRaisesRegex(ValueError, "ground-truth fingerprint mismatch"):
            evaluation.score(self.output)

    def test_command_runner_resumes_and_enforces_experiment_identity(self):
        runner = self.output / "runner.py"
        runner.write_text("import json,sys\nx=json.load(sys.stdin)\nprint(json.dumps({'findings': []}))\n")
        argv = shlex.join([sys.executable, str(runner)])
        # Use fresh response records from the actual runner instead of synthetic records.
        shutil.rmtree(self.output / "responses")
        with contextlib.redirect_stdout(io.StringIO()):
            evaluation.run(self.output, argv, "empty-scripted-reviewer")
        original = (self.output / "responses/packs-bug-case.json").read_bytes()
        with contextlib.redirect_stdout(io.StringIO()):
            evaluation.run(self.output, argv, "empty-scripted-reviewer")
        self.assertEqual(original, (self.output / "responses/packs-bug-case.json").read_bytes())
        with self.assertRaisesRegex(ValueError, "mix reviewers"):
            evaluation.run(self.output, argv, "another-reviewer")
        self.assertEqual(evaluation.score(self.output)["metrics"]["packs"]["recall"], 0)

    def test_timeouts_and_malformed_reviewer_output_remain_failures(self):
        shutil.rmtree(self.output / "responses")
        command = shlex.join([sys.executable, "-c", "import time; time.sleep(1)"])
        with contextlib.redirect_stdout(io.StringIO()):
            evaluation.run(self.output, command, "timeout-test", timeout=0.01)
        self.assertEqual(evaluation.score(self.output)["metrics"]["packs"]["failed"], 2)
        shutil.rmtree(self.output / "responses")
        command = shlex.join([sys.executable, "-c", "print('null')"])
        with contextlib.redirect_stdout(io.StringIO()):
            evaluation.run(self.output, command, "invalid-protocol")
        self.assertEqual(evaluation.score(self.output)["metrics"]["packs"]["failed"], 2)


@unittest.skipUnless((Path(__file__).resolve().parents[1] / "target/debug/sonar").is_file(), "build Sonar before the CLI integration test")
class ReviewPreparationIntegrationTests(unittest.TestCase):
    def test_all_three_comparison_inputs_are_blinded_and_executable(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "experiment"
            binary = Path(__file__).resolve().parents[1] / "target/debug/sonar"
            result = evaluation.prepare(root, binary)
            self.assertEqual(result["cases"], 6)
            self.assertEqual(result["trials"], 18)
            manifest, truth = evaluation.read_json(root / "manifest.json"), evaluation.read_json(root / "private/truth.json")
            for trial in manifest["trials"]:
                value = evaluation.read_json(root / trial["input"])
                self.assertEqual(trial["input_digest"], evaluation.digest(value))
                self.assertNotIn("private/", json.dumps(value))
                self.assertNotIn("issues", value)
                self.assertNotIn("version", value["context"])  # no buggy/control label
                self.assertNotIn("oracle", json.dumps(value))
                self.assertEqual(set(value["snapshots"]), {"baseline", "current"})
                if trial["variant"] == "packs":
                    for pack in value["context"]["review_packs"]["packs"]:
                        self.assertLessEqual(pack["estimated_tokens"], 16000)
            # Local Python import facts now bring unchanged policy into review packs;
            # the diff-only strategy still cannot expose that supporting file.
            for case_id, case in truth.items():
                if case["version"] != "buggy":
                    continue
                for helper in case["issues"][0]["required_context"]:
                    visibility = {t["variant"]: evaluation.shown(t["visible"], helper) for t in manifest["trials"] if t["case"] == case_id}
                    self.assertEqual(visibility, {"packs": True, "diff": False, "full": True})
            with self.assertRaisesRegex(ValueError, "already exists"):
                evaluation.prepare(root, binary)


if __name__ == "__main__":
    unittest.main()
