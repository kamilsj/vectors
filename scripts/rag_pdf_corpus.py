#!/usr/bin/env python3
"""Create a repeatable PDF corpus, then measure source retrieval through the API.

Uses only Python's standard library. No provider calls during corpus creation.
Evaluation invokes the collection's configured embedding/reranking providers.
"""

import argparse
import json
import math
import os
from pathlib import Path
import statistics
import textwrap
import time
import urllib.error
import urllib.parse
import urllib.request


def pdf_bytes(pages):
    """Write small ASCII text PDFs with real page objects and an xref table."""
    objects = [b"<< /Type /Catalog /Pages 2 0 R >>", b"",
               b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"]
    page_ids = []
    for title, paragraphs in pages:
        lines = [title, ""]
        for paragraph in paragraphs:
            lines.extend(textwrap.wrap(paragraph, width=84))
            lines.append("")
        if len(lines) > 46:
            raise ValueError("fixture page exceeds printable height")
        commands = ["BT /F1 11 Tf 15 TL 50 780 Td"]
        for index, line in enumerate(lines):
            escaped = line.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
            commands.append(("T* " if index else "") + f"({escaped}) Tj")
        commands.append("ET")
        stream = "\n".join(commands).encode("ascii")
        page_id, content_id = len(objects) + 1, len(objects) + 2
        page_ids.append(page_id)
        objects.append((f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] "
                        f"/Resources << /Font << /F1 3 0 R >> >> /Contents {content_id} 0 R >>").encode())
        objects.append(f"<< /Length {len(stream)} >>\nstream\n".encode() + stream + b"\nendstream")
    kids = " ".join(f"{number} 0 R" for number in page_ids)
    objects[1] = f"<< /Type /Pages /Kids [{kids}] /Count {len(page_ids)} >>".encode()
    output = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = [0]
    for number, obj in enumerate(objects, 1):
        offsets.append(len(output))
        output.extend(f"{number} 0 obj\n".encode() + obj + b"\nendobj\n")
    start = len(output)
    output.extend(f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode())
    for offset in offsets[1:]:
        output.extend(f"{offset:010d} 00000 n \n".encode())
    output.extend((f"trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\n"
                   f"startxref\n{start}\n%%EOF\n").encode())
    return bytes(output)


def create_corpus(destination, count):
    # New directory only: never overwrite an existing user's collection.
    destination.mkdir(parents=True, exist_ok=False)
    pdf_dir = destination / "pdfs"
    pdf_dir.mkdir()
    questions = []
    for number in range(1, count + 1):
        project = f"ATLAS-{number:05d}"
        filename = f"manual-{number:05d}.pdf"
        code = f"ORBIT-{(number * 7919) % 99991:05d}"
        retention = 20 + number % 73
        pages = [
            (f"{project} - Operations manual - Page 1", [
                "Synthetic test document. These project names and instructions are invented.",
                f"The recovery authorization code for project {project} is {code}. "
                "Operators must confirm this code before starting a recovery procedure.",
                f"Project {project} stores its encrypted backups in region R{number % 17:02d}. "
                "The recovery sequence is: verify the authorization code, check the journal, "
                "restore the latest snapshot, then replay committed entries.",
                "This page describes recovery. The next page gives the retention policy.",
            ]),
            (f"{project} - Retention policy - Page 2", [
                f"Project {project} retains backup snapshots for exactly {retention} days. "
                "The retention clock starts when a snapshot is acknowledged by storage.",
                "Keep audit records after the backup expires. Deleting a snapshot does not "
                "delete its audit record. Exceptions require a documented approval.",
                f"Use the recovery authorization code from page 1 of this {project} manual. "
                "Codes from other project manuals must never be substituted.",
            ]),
        ]
        (pdf_dir / filename).write_bytes(pdf_bytes(pages))
        questions.extend([
            {"question": f"What is the recovery authorization code for {project}?",
             "expected_text": code, "expected_file": filename, "expected_page": 1},
            {"question": f"How many days does {project} retain backup snapshots?",
             "expected_text": f"{retention} days", "expected_file": filename, "expected_page": 2},
        ])
    (destination / "questions.json").write_text(json.dumps(questions, indent=2) + "\n")
    (destination / "README.txt").write_text(
        f"{count} synthetic PDFs, 2 pages each; no external data or API calls.\n"
        "In Vectors: Playground > Documents > choose the pdfs folder, then Start import.\n"
        "Use Chat to ask questions and inspect retrieved sources.\n"
        "Choose Evaluate in Chat, import questions.json, and Run evaluation.\n"
        "The scripts/rag_pdf_corpus.py evaluate command can also measure retrieval.\n"
        "These simple documents test ingestion/provenance; they do not model production quality.\n")
    print(f"Created {count} PDFs and {len(questions)} known-answer questions in {destination}")


def evaluate(args):
    if args.output.exists():
        raise ValueError("output report already exists; choose a new path before making provider calls")
    if not args.output.parent.is_dir():
        raise ValueError("output report directory must exist")
    parsed = urllib.parse.urlsplit(args.url)
    if parsed.scheme not in ("http", "https") or not parsed.hostname or parsed.username or parsed.password:
        raise ValueError("--url must be an http(s) server URL without embedded credentials")
    if parsed.query or parsed.fragment:
        raise ValueError("--url must not contain a query or fragment")
    if parsed.scheme == "http" and parsed.hostname not in ("127.0.0.1", "localhost", "::1"):
        raise ValueError("use HTTPS for a remote server")
    questions = json.loads(args.questions.read_text())
    if not isinstance(questions, list) or not questions:
        raise ValueError("questions must be a nonempty JSON list")
    # Evenly spaced questions exercise the whole corpus, rather than only its first files.
    size = min(args.limit, len(questions))
    selected = [questions[round(i * (len(questions) - 1) / max(1, size - 1))] for i in range(size)]
    for item in selected:
        if (not isinstance(item, dict)
                or any(not isinstance(item.get(key), str) or not item[key]
                       for key in ("question", "expected_text", "expected_file"))
                or len(item["question"].encode()) > 8191
                or "\0" in item["question"]
                or type(item.get("expected_page")) is not int or item["expected_page"] < 1):
            raise ValueError("every question needs nonempty question/expected_text/expected_file strings and a positive expected_page")
    endpoint = args.url.rstrip("/") + "/v1/graph/collections/" + urllib.parse.quote(args.collection, safe="") + "/retrieve"
    headers = {"Content-Type": "application/json"}
    token = os.environ.get("VECTORS_API_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    # Do not forward the bearer token across redirects to another host.
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            return None
    opener = urllib.request.build_opener(NoRedirect)
    results = []
    print(f"Evaluating {size} questions sequentially (provider calls may incur costs).")
    for item in selected:
        body = {"text": item["question"], "max_results": args.top_k, "candidate_limit": max(40, args.top_k),
                "max_hops": args.hops, "max_seeds_per_document": 2}
        start = time.perf_counter()
        request = urllib.request.Request(endpoint, json.dumps(body).encode(), headers)
        try:
            with opener.open(request, timeout=args.timeout) as response:
                data = response.read(4 * 1024 * 1024 + 1)
                if len(data) > 4 * 1024 * 1024:
                    raise ValueError("retrieval response exceeds 4 MiB")
                result = json.loads(data)
        except urllib.error.HTTPError as error:
            raise ValueError(f"evaluation stopped at question {len(results) + 1}: HTTP {error.code}; no retries") from None
        except urllib.error.URLError:
            raise ValueError(f"evaluation stopped at question {len(results) + 1}: connection failed; no retries") from None
        elapsed_ms = (time.perf_counter() - start) * 1000
        ranks = []
        for rank, hit in enumerate(result.get("hits", []), 1):
            citation = hit.get("citation", {})
            # Citation source points to extracted PDF page, not binary byte offsets.
            source = citation.get("source", hit.get("source", ""))
            expected_source = item["expected_file"] + f"#page={item['expected_page']}"
            if ((source == expected_source or source.endswith("/" + expected_source))
                    and item["expected_text"] in hit.get("text", "")):
                ranks.append(rank)
        results.append({**item, "matched_rank": min(ranks) if ranks else None,
                        "elapsed_ms": round(elapsed_ms, 3), "revision": result.get("revision"),
                        "candidate_count": result.get("candidate_count"),
                        "embedding_usage": result.get("embedding_usage"),
                        "timings": result.get("timings")})
    revisions = {r["revision"] for r in results}
    latencies = sorted(r["elapsed_ms"] for r in results)
    report = {"collection": args.collection, "questions": size, "top_k": args.top_k,
              "max_hops": args.hops, "stable_revision": len(revisions) == 1,
              "source_and_answer_recall": sum(r["matched_rank"] is not None for r in results) / size,
              "mean_reciprocal_rank": sum(1 / r["matched_rank"] if r["matched_rank"] else 0 for r in results) / size,
              "latency_ms": {"median": statistics.median(latencies), "p95": latencies[math.ceil(.95 * size) - 1]},
              "scope": "Synthetic exact-fact source retrieval, including provider/network latency; not answer-generation quality.",
              "results": results}
    # Explicit output, opened exclusively so a prior report cannot be lost.
    with args.output.open("x") as output:
        output.write(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: value for key, value in report.items() if key != "results"}, indent=2))
    print(f"Saved report to {args.output}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    create = commands.add_parser("create", help="generate PDFs and known-answer questions without provider calls")
    create.add_argument("--output", type=Path, required=True)
    create.add_argument("--files", type=int, default=100)
    test = commands.add_parser("evaluate", help="test an already imported corpus (uses configured providers)")
    test.add_argument("--url", default="http://127.0.0.1:8080")
    test.add_argument("--collection", required=True)
    test.add_argument("--questions", type=Path, required=True)
    test.add_argument("--output", type=Path, required=True)
    test.add_argument("--limit", type=int, default=20)
    test.add_argument("--top-k", type=int, default=10)
    test.add_argument("--hops", type=int, choices=range(4), default=1)
    test.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    try:
        if args.command == "create":
            if not 1 <= args.files <= 100_000:
                parser.error("--files must be in 1..100000; collection capacity still applies")
            create_corpus(args.output, args.files)
        else:
            if not 1 <= args.limit <= 10_000 or not 1 <= args.top_k <= 100 or args.timeout <= 0:
                parser.error("use --limit 1..10000, --top-k 1..100 and a positive --timeout")
            evaluate(args)
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f"Error: {error}\n")


if __name__ == "__main__":
    main()
