"""Source-evaluation checks without a server or provider calls."""
import argparse
from contextlib import redirect_stdout
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("corpus", Path(__file__).parents[1] / "scripts/rag_pdf_corpus.py")
corpus = importlib.util.module_from_spec(spec)
spec.loader.exec_module(corpus)


class Response(io.BytesIO):
    pass


class CorpusEvaluationTests(unittest.TestCase):
    def test_requires_fact_file_and_page_and_preserves_rank(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            question = {"question": "Which code?", "expected_text": "ORBIT-00001",
                        "expected_file": "manual-00001.pdf", "expected_page": 2}
            questions = root / "questions.json"
            questions.write_text(json.dumps([question, question]))
            args = argparse.Namespace(url="http://127.0.0.1:8080", collection="lab", questions=questions,
                                      output=root / "report.json", limit=2, top_k=50, hops=1, timeout=3)
            responses = [
                {"revision": 1, "hits": [
                    {"source": "manual-00001.pdf#page=1", "text": "ORBIT-00001"},
                    {"source": "folder/manual-00001.pdf#page=2", "text": "Code: ORBIT-00001"}]},
                {"revision": 2, "hits": [
                    {"source": "manual-00001.pdf#page=2", "text": "wrong answer"},
                    {"source": "wrong-manual-00001.pdf#page=2", "text": "ORBIT-00001"}]},
            ]
            sent = []

            class Opener:
                def open(self, request, timeout):
                    sent.append(json.loads(request.data))
                    return Response(json.dumps(responses.pop(0)).encode())

            with patch.object(corpus.urllib.request, "build_opener", return_value=Opener()), redirect_stdout(io.StringIO()):
                corpus.evaluate(args)
            report = json.loads(args.output.read_text())
            self.assertEqual(report["source_and_answer_recall"], .5)
            self.assertEqual(report["mean_reciprocal_rank"], .25)
            self.assertFalse(report["stable_revision"])
            self.assertEqual(sent[0]["candidate_limit"], 50)

    def test_existing_output_and_bad_questions_fail_before_requests(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            args = argparse.Namespace(url="http://127.0.0.1:8080", questions=root / "questions.json",
                                      output=root / "report.json", limit=2)
            args.output.write_text("keep")
            with self.assertRaisesRegex(ValueError, "already exists"):
                corpus.evaluate(args)
            self.assertEqual(args.output.read_text(), "keep")
            args.output = root / "new-report.json"
            args.questions.write_text('[{"question":"incomplete"}]')
            with patch.object(corpus.urllib.request, "build_opener") as opener:
                with self.assertRaisesRegex(ValueError, "every question needs"):
                    corpus.evaluate(args)
                opener.assert_not_called()

    def test_remote_plaintext_credentials_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            args = argparse.Namespace(url="http://example.com", output=Path(directory) / "report.json")
            with self.assertRaisesRegex(ValueError, "use HTTPS"):
                corpus.evaluate(args)


if __name__ == "__main__":
    unittest.main()
