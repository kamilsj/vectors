#!/usr/bin/env python3
"""Run bounded chatbot answer checks against an imported collection (provider charges apply).

Exact status, fact and source checks are regression signals, not semantic entailment
scores. Each case specifies its complete history, so failures cannot contaminate
later cases. Reports include source passages and may contain private document text.
"""

import argparse
import json
import math
import os
from pathlib import Path
import re
import statistics
import time
import urllib.error
import urllib.parse
import urllib.request

MAX_BYTES = 4 * 1024 * 1024
STATUSES = {"answered", "insufficient_evidence", "clarification_needed", "no_sources"}


def load_cases(path):
    if path.stat().st_size > MAX_BYTES:
        raise ValueError("case file exceeds 4 MiB")
    cases = json.loads(path.read_text())
    if not isinstance(cases, list) or not 1 <= len(cases) <= 1000:
        raise ValueError("provide 1..1000 cases; each run is limited to at most 100")
    ids = set()
    for case in cases:
        if not isinstance(case, dict) or set(case) - {"id", "request", "expected_status", "answer_contains", "answer_excludes", "expected_sources"}:
            raise ValueError("unknown case field")
        if not isinstance(case.get("id"), str) or not case["id"].strip() or case["id"] in ids:
            raise ValueError("every case needs a unique nonempty id")
        ids.add(case["id"])
        request = case.get("request")
        if not isinstance(request, dict) or not isinstance(request.get("text"), str) or not request["text"].strip():
            raise ValueError("every case needs request.text")
        if request.get("mode", "answer") != "answer" or request.get("grounding", "strict") != "strict":
            raise ValueError("answer evaluation requires answer mode and strict grounding")
        if case.get("expected_status") not in STATUSES:
            raise ValueError("expected_status must be answered, insufficient_evidence, clarification_needed or no_sources")
        for key in ("answer_contains", "answer_excludes", "expected_sources"):
            values = case.get(key, [])
            if not isinstance(values, list) or any(not isinstance(value, str) or not value.strip() for value in values):
                raise ValueError(f"{key} must be a list of nonempty strings")
        if case["expected_status"] == "answered" and (not case.get("answer_contains") or not case.get("expected_sources")):
            raise ValueError("answered cases need expected facts and cited sources")
    return cases


def score(case, response):
    """Keep answer facts and cited-source correctness separate from retrieval recall."""
    if not isinstance(response, dict):
        raise ValueError("chat response must be an object")
    answer = response.get("answer")
    text = answer if isinstance(answer, str) else ""
    cited = response.get("cited_labels", [])
    citations = response.get("citations", [])
    retrieval = response.get("retrieval", {})
    if not isinstance(cited, list) or not isinstance(citations, list) or not isinstance(retrieval, dict):
        raise ValueError("invalid chat citation fields")
    sources = {item.get("source") for item in citations if isinstance(item, dict)
               and item.get("label") in cited and isinstance(item.get("source"), str)}
    missing = [fact for fact in case.get("answer_contains", []) if fact.casefold() not in text.casefold()]
    forbidden = [fact for fact in case.get("answer_excludes", []) if fact.casefold() in text.casefold()]
    missing_sources = [source for source in case.get("expected_sources", [])
                       if not any(actual == source or actual.endswith("/" + source) for actual in sources)]
    status_matches = response.get("answer_status") == case["expected_status"]
    citations_valid = case["expected_status"] != "answered"
    if case["expected_status"] == "answered":
        evidence = response.get("evidence", [])
        if not isinstance(evidence, list) or not isinstance(retrieval, dict) or not isinstance(retrieval.get("hits", []), list):
            raise ValueError("invalid chat evidence fields")
        valid_citations = [item for item in citations if isinstance(item, dict)
                           and isinstance(item.get("label"), str) and isinstance(item.get("chunk_id"), str)]
        known = {item["label"]: item for item in valid_citations}
        hits = {hit["chunk_id"]: hit for hit in retrieval.get("hits", [])
                if isinstance(hit, dict) and isinstance(hit.get("chunk_id"), str)}
        labels = set(re.findall(r"\[(S[1-9]\d*)\]", text))
        quotes_match = bool(evidence)
        evidence_labels = set()
        for item in evidence:
            if not isinstance(item, dict) or not isinstance(item.get("label"), str) or not isinstance(item.get("quote"), str):
                quotes_match = False
                continue
            evidence_labels.add(item["label"])
            passage = hits.get(known.get(item["label"], {}).get("chunk_id"), {}).get("text", "")
            if (not isinstance(passage, str) or not item["quote"].strip()
                    or len(item["quote"].strip()) < min(16, len(passage.strip()))
                    or item["quote"] not in passage):
                quotes_match = False
        citations_valid = (response.get("citation_status") == "valid_labels" and bool(labels)
                           and all(isinstance(label, str) for label in cited)
                           and set(cited) == labels == evidence_labels
                           and labels.issubset(known) and len(known) == len(valid_citations)
                           and "[S" not in re.sub(r"\[S[1-9]\d*\]", "", text) and quotes_match)
    voice_required = case["expected_status"] == "answered" and case["request"].get("answer_style") == "voice"
    speech = response.get("speech_text")
    voice_available = not voice_required or (isinstance(speech, str) and bool(speech.strip()))
    expected_speech = text
    for label in cited:
        if isinstance(label, str):
            expected_speech = expected_speech.replace(f"[{label}]", "")
    voice_matches_answer = not voice_required or (voice_available and speech == " ".join(expected_speech.split()))
    return {"passed": status_matches and citations_valid and voice_available and voice_matches_answer and not (missing or forbidden or missing_sources),
            "status_matches": status_matches, "citations_valid": citations_valid,
            "voice_available": voice_available, "voice_matches_answer": voice_matches_answer, "missing_facts": missing,
            "forbidden_facts": forbidden, "missing_sources": missing_sources}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def evaluate(args):
    if not 1 <= args.limit <= 100 or not math.isfinite(args.timeout) or args.timeout <= 0:
        raise ValueError("use limit 1..100 and a finite positive timeout")
    parsed = urllib.parse.urlsplit(args.url)
    if parsed.scheme not in ("http", "https") or not parsed.hostname or parsed.username or parsed.password or parsed.query or parsed.fragment:
        raise ValueError("use an http(s) server URL without embedded credentials, query or fragment")
    if parsed.scheme == "http" and parsed.hostname not in ("127.0.0.1", "localhost", "::1"):
        raise ValueError("use HTTPS for a remote server")
    cases = load_cases(args.cases)[:args.limit]
    endpoint = args.url.rstrip("/") + "/v1/graph/collections/" + urllib.parse.quote(args.collection, safe="") + "/chat"
    headers = {"Content-Type": "application/json"}
    if os.environ.get("VECTORS_API_TOKEN"):
        headers["Authorization"] = "Bearer " + os.environ["VECTORS_API_TOKEN"]
    opener = urllib.request.build_opener(NoRedirect)
    report = {"collection": args.collection, "planned_cases": len(cases), "completed": False, "results": [],
              "scope": "Deterministic answer-status, literal-fact and cited-source regression checks; not semantic factual-accuracy or retrieval-recall measurement."}
    # Reserve the report before any paid work and persist every result. A partial
    # report survives provider errors or interruption; existing files are never overwritten.
    descriptor = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        def save():
            output.seek(0)
            output.write(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
            output.truncate()
            output.flush()

        save()
        print(f"Running {len(cases)} answer cases sequentially. Provider charges may apply; no retries.")
        for case in cases:
            request_body = {"grounding": "strict", **case["request"]}
            started = time.perf_counter()
            try:
                request = urllib.request.Request(endpoint, json.dumps(request_body, allow_nan=False).encode(), headers)
                with opener.open(request, timeout=args.timeout) as response:
                    data = response.read(MAX_BYTES + 1)
                    if len(data) > MAX_BYTES:
                        raise ValueError("chat response exceeds 4 MiB")
                    response_data = json.loads(data)
                checks = score(case, response_data)
            except (urllib.error.HTTPError, urllib.error.URLError, OSError, ValueError, TypeError) as error:
                # Never copy provider/server bodies, tokens, or arbitrary exception text into reports.
                failure = f"HTTP {error.code}" if isinstance(error, urllib.error.HTTPError) else "transport or response failure"
                report["error"] = {"case_id": case["id"], "kind": failure, "retried": False}
                save()
                raise ValueError(f"evaluation stopped at {case['id']}: {failure}; partial report saved; no retries") from None
            report["results"].append({"id": case["id"], "request": request_body,
                                      "expected_status": case["expected_status"], "checks": checks,
                                      "elapsed_ms": round((time.perf_counter() - started) * 1000, 3),
                                      "response": response_data})
            save()
        results = report["results"]
        latencies = sorted(result["elapsed_ms"] for result in results)
        report["completed"] = True
        report["pass_rate"] = sum(result["checks"]["passed"] for result in results) / len(results)
        report["latency_ms"] = {"median": statistics.median(latencies), "p95": latencies[math.ceil(.95 * len(results)) - 1]}
        revisions = [result["response"].get("retrieval", {}).get("revision") for result in results]
        report["stable_revision"] = all(type(revision) is int for revision in revisions) and len(set(revisions)) == 1
        save()
    print(f"Passed {sum(result['checks']['passed'] for result in results)}/{len(results)} cases. Saved {args.output}")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:8080")
    parser.add_argument("--collection", required=True)
    parser.add_argument("--cases", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--limit", type=int, default=20)
    parser.add_argument("--timeout", type=float, default=240)
    args = parser.parse_args()
    try:
        report = evaluate(args)
    except (OSError, ValueError) as error:
        parser.exit(1, f"Error: {error}\n")
    if report["pass_rate"] < 1:
        parser.exit(2, "One or more answer checks failed. Review the saved evidence.\n")


if __name__ == "__main__":
    main()
