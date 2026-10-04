"""Answer evaluation protocol tests; no server, credentials or provider calls."""
import argparse
from contextlib import redirect_stdout
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import urllib.error

spec = importlib.util.spec_from_file_location("chat_eval", Path(__file__).parents[1] / "scripts/rag_chat_eval.py")
chat_eval = importlib.util.module_from_spec(spec)
spec.loader.exec_module(chat_eval)


def case():
    return {"id": "supported", "request": {"text": "Retention?"}, "expected_status": "answered",
            "answer_contains": ["21 days"], "answer_excludes": ["99 days"],
            "expected_sources": ["manual.pdf#page=2"]}


def result():
    return {"answer": "21 days. [S1]", "answer_status": "answered", "citation_status": "valid_labels",
            "cited_labels": ["S1"], "evidence": [{"label": "S1", "quote": "Backups are retained for 21 days."}],
            "citations": [{"label": "S1", "chunk_id": "chunk1", "source": "folder/manual.pdf#page=2"}],
            "retrieval": {"revision": 5, "hits": [{"chunk_id": "chunk1", "text": "Backups are retained for 21 days."}]}}


class ChatEvaluationTests(unittest.TestCase):
    def test_checks_facts_and_actual_cited_sources_independently(self):
        expected, response = case(), result()
        self.assertTrue(chat_eval.score(expected, response)["passed"])
        response["answer"] = "99 days [S1]"
        checks = chat_eval.score(expected, response)
        self.assertFalse(checks["passed"])
        self.assertEqual(checks["missing_facts"], ["21 days"])
        self.assertEqual(checks["forbidden_facts"], ["99 days"])
        response = result()
        response["cited_labels"] = ["S2"]
        self.assertEqual(chat_eval.score(expected, response)["missing_sources"], ["manual.pdf#page=2"])
        response["citations"][0]["source"] = "wrong-manual.pdf#page=2"
        self.assertFalse(chat_eval.score(expected, response)["passed"])

    def test_retrieved_sources_do_not_count_as_answer_citations(self):
        response = result()
        response["cited_labels"] = []
        response["retrieval"]["hits"] = [{"source": "manual.pdf#page=2", "text": "21 days"}]
        self.assertFalse(chat_eval.score(case(), response)["passed"])

    def test_extra_fabricated_labels_or_quotes_cannot_pass(self):
        response = result()
        response["answer"] += " [S99]"
        response["cited_labels"].append("S99")
        self.assertFalse(chat_eval.score(case(), response)["passed"])
        response = result()
        response["evidence"][0]["quote"] = "Backups are retained for 99 days."
        self.assertFalse(chat_eval.score(case(), response)["passed"])

    def test_voice_facts_must_match_the_displayed_answer(self):
        expected, response = case(), result()
        expected["request"]["answer_style"] = "voice"
        response["speech_text"] = "99 days."
        self.assertFalse(chat_eval.score(expected, response)["passed"])
        response["speech_text"] = "21 days."
        self.assertTrue(chat_eval.score(expected, response)["passed"])

    def test_abstention_and_voice_are_checked(self):
        expected = {"request": {"text": "price?"}, "expected_status": "insufficient_evidence"}
        response = {"answer": "Not enough evidence.", "answer_status": "insufficient_evidence"}
        self.assertTrue(chat_eval.score(expected, response)["passed"])
        expected["request"]["answer_style"] = "voice"
        self.assertTrue(chat_eval.score(expected, response)["passed"])
        response["speech_text"] = "Not enough evidence."
        self.assertTrue(chat_eval.score(expected, response)["passed"])
        response["answer_status"] = "answered"
        self.assertFalse(chat_eval.score(expected, response)["passed"])
        expected = case()
        expected["request"]["answer_style"] = "voice"
        self.assertFalse(chat_eval.score(expected, result())["passed"])

    def test_no_retry_and_partial_report_on_second_request_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cases = [case(), {**case(), "id": "second"}]
            (root / "cases.json").write_text(json.dumps(cases))
            args = argparse.Namespace(url="http://127.0.0.1:8080", collection="lab", cases=root / "cases.json",
                                      output=root / "report.json", limit=2, timeout=10)
            requests = []

            class Opener:
                def open(self, request, timeout):
                    requests.append(request)
                    if len(requests) == 2:
                        raise urllib.error.HTTPError(request.full_url, 429, "sensitive-provider-message", {}, None)
                    return io.BytesIO(json.dumps(result()).encode())

            with patch.object(chat_eval.urllib.request, "build_opener", return_value=Opener()), redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(ValueError, "HTTP 429"):
                    chat_eval.evaluate(args)
            report = json.loads(args.output.read_text())
            self.assertFalse(report["completed"])
            self.assertEqual(len(report["results"]), 1)
            self.assertEqual(len(requests), 2)
            self.assertFalse(report["error"]["retried"])
            self.assertNotIn("sensitive-provider-message", args.output.read_text())
            self.assertEqual(json.loads(requests[0].data)["grounding"], "strict")
            with patch.object(chat_eval.urllib.request, "build_opener", return_value=Opener()):
                with self.assertRaises(FileExistsError):
                    chat_eval.evaluate(args)
            self.assertEqual(len(requests), 2)

    def test_invalid_cases_and_remote_http_fail_before_network(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            args = argparse.Namespace(url="http://example.com", collection="lab", cases=root / "cases.json",
                                      output=root / "report.json", limit=2, timeout=10)
            with self.assertRaisesRegex(ValueError, "HTTPS"):
                chat_eval.evaluate(args)
            args.url = "http://localhost:8080"
            bad = case()
            bad["request"]["grounding"] = "standard"
            args.cases.write_text(json.dumps([bad]))
            with patch.object(chat_eval.urllib.request, "build_opener") as opener:
                with self.assertRaisesRegex(ValueError, "strict grounding"):
                    chat_eval.evaluate(args)
                opener.assert_not_called()
            self.assertFalse(args.output.exists())

    def test_success_report_and_fixture_schema(self):
        fixture = Path(__file__).parent / "fixtures/chatbot-quality.json"
        self.assertEqual(len(chat_eval.load_cases(fixture)), 6)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "cases.json").write_text(json.dumps([case()]))
            args = argparse.Namespace(url="http://localhost:8080", collection="lab", cases=root / "cases.json",
                                      output=root / "report.json", limit=1, timeout=10)
            class Opener:
                def open(self, request, timeout):
                    return io.BytesIO(json.dumps(result()).encode())
            with patch.object(chat_eval.urllib.request, "build_opener", return_value=Opener()), redirect_stdout(io.StringIO()):
                report = chat_eval.evaluate(args)
            self.assertTrue(report["completed"])
            self.assertTrue(report["stable_revision"])
            self.assertEqual(report["pass_rate"], 1)


if __name__ == "__main__":
    unittest.main()
