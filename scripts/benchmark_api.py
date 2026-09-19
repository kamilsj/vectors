#!/usr/bin/env python3
"""Compare two local server binaries over real keep-alive HTTP connections.

Only synthetic in-memory databases are used. No installed server or user data is
touched. Timings include HTTP, request decoding, database work, and response
encoding/transfer; client JSON encoding and parsing are outside the timer.
"""

import argparse
import hashlib
import http.client
import json
import math
import os
import platform
import signal
import socket
import statistics
import subprocess
import tempfile
import time
from pathlib import Path


def body(value):
    return json.dumps(value, separators=(",", ":"), allow_nan=False).encode()


def vector(index, dimensions):
    return [((index * 17 + col * 13) % 1021 - 510) / 512 for col in range(dimensions)]


def request(connection, path, payload):
    started = time.perf_counter_ns()
    connection.request("POST", path, body=payload, headers={"Content-Type": "application/json"})
    response = connection.getresponse()
    data = response.read()
    elapsed_ms = (time.perf_counter_ns() - started) / 1e6
    if response.status != 200:
        raise RuntimeError(f"{path}: HTTP {response.status}: {data[:500]!r}")
    return elapsed_ms, data


def sql(connection, statement):
    return request(connection, "/v1/sql", body({"sql": statement}))[1]


def seed(connection, name, rows, dimensions):
    sql(connection, f"CREATE TABLE {name} (id INTEGER PRIMARY KEY, category INTEGER, embedding VECTOR({dimensions}))")
    for start in range(0, rows, 256):
        data = [{"id": index, "category": index % 64, "embedding": vector(index, dimensions)}
                for index in range(start, min(start + 256, rows))]
        request(connection, f"/v1/tables/{name}/rows", body({"rows": data}))


def percentile(samples, fraction):
    return sorted(samples)[max(0, math.ceil(len(samples) * fraction) - 1)]


def summarize(samples, digest, response_bytes):
    return {"p50_ms": statistics.median(samples), "p95_ms": percentile(samples, .95),
            "samples_ms": samples, "responses_sha256": digest.hexdigest(),
            "response_bytes": response_bytes}


def benchmark(binary, args):
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    environment = {key: value for key, value in os.environ.items() if not key.startswith("VECTORS_")}
    environment.update({"RAYON_NUM_THREADS": str(args.threads), "VECTORS_HTTP_WORKERS": "2",
                        "VECTORS_HTTP_MAX_BLOCKING_THREADS_PER_WORKER": "2",
                        "VECTORS_MAX_CONCURRENT_DATABASE_TASKS": "4"})
    with tempfile.TemporaryFile() as log:
        process = subprocess.Popen([str(binary), "--port", str(port), "--compute", "cpu"],
                                   env=environment, stdout=log, stderr=log)
        connection = None
        try:
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    log.seek(0)
                    raise RuntimeError(log.read().decode(errors="replace"))
                try:
                    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
                    connection.request("GET", "/healthz")
                    response = connection.getresponse()
                    response.read()
                    if response.status == 200:
                        break
                except OSError:
                    connection.close()
                    time.sleep(.025)
            else:
                raise RuntimeError("test server did not become ready")

            connection.request("GET", "/v1/settings/server")
            response = connection.getresponse()
            settings = json.loads(response.read())
            assert response.status == 200 and settings["storage"] == "memory"
            assert settings["compute"]["device"] == "cpu"
            assert settings["capacity"]["workers"] == 2
            assert settings["capacity"]["max_blocking_threads_per_worker"] == 2
            assert settings["capacity"]["max_concurrent_database_tasks"] == 4

            seed(connection, "small", 32, 1536)
            seed(connection, "documents", args.rows, args.dimensions)
            sql(connection, "CREATE INDEX category_idx ON documents(category)")
            results = {}
            for label, table, dimensions, filters, select in [
                ("small_high_dimension_search", "small", 1536, [], ["id"]),
                ("indexed_search", "documents", args.dimensions,
                 [{"column": "category", "operator": "eq", "value": 7}], ["id"]),
                ("full_scan_search", "documents", args.dimensions, [], ["id"]),
            ]:
                samples, digest, sizes = [], hashlib.sha256(), []
                for iteration in range(args.warmup + args.iterations):
                    payload = body({"table": table, "vector_column": "embedding",
                                    "query": vector(iteration + 43, dimensions), "select": select,
                                    "filters": filters, "metric": "cosine", "limit": 10})
                    elapsed, data = request(connection, "/v1/vector/search", payload)
                    decoded = json.loads(data)
                    assert decoded["row_count"] == min(10, 32 if table == "small" else
                                                        (len(range(7, args.rows, 64)) if filters else args.rows))
                    if iteration >= args.warmup:
                        samples.append(elapsed)
                        digest.update(data)
                        sizes.append(len(data))
                results[label] = summarize(samples, digest, sizes)

            samples, digest, sizes = [], hashlib.sha256(), []
            payload = body({"sql": f"SELECT id, embedding FROM documents LIMIT {min(1000, args.rows)}"})
            for iteration in range(args.warmup + args.iterations):
                elapsed, data = request(connection, "/v1/sql", payload)
                assert json.loads(data)["results"][0]["row_count"] == min(1000, args.rows)
                if iteration >= args.warmup:
                    samples.append(elapsed)
                    digest.update(data)
                    sizes.append(len(data))
            results["sql_vector_response"] = summarize(samples, digest, sizes)

            columns = ", ".join(f"m{index} TEXT" for index in range(32))
            create = f"CREATE TABLE ingest (id INTEGER PRIMARY KEY, {columns}, embedding VECTOR(64))"
            rows = [{"id": index, "embedding": vector(index, 64),
                     **{f"m{col}": f"value-{index}-{col}" for col in range(32)}}
                    for index in range(args.ingest_rows)]
            payload = body({"rows": rows, "normalize_vectors": True})
            samples, digest, sizes = [], hashlib.sha256(), []
            for iteration in range(args.warmup + args.iterations):
                sql(connection, create)
                elapsed, data = request(connection, "/v1/tables/ingest/rows", payload)
                assert json.loads(data)["results"][0]["rows_affected"] == args.ingest_rows
                if iteration >= args.warmup:
                    samples.append(elapsed)
                    digest.update(data)
                    sizes.append(len(data))
                sql(connection, "DROP TABLE ingest")
            results["wide_normalized_ingestion"] = summarize(samples, digest, sizes)
            return results
        finally:
            if connection is not None:
                connection.close()
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--iterations", type=int, default=30)
    parser.add_argument("--warmup", type=int, default=5)
    parser.add_argument("--rows", type=int, default=4096)
    parser.add_argument("--dimensions", type=int, default=384)
    parser.add_argument("--ingest-rows", type=int, default=256)
    parser.add_argument("--threads", type=int, default=4)
    args = parser.parse_args()
    if min(args.runs, args.iterations, args.rows, args.dimensions, args.ingest_rows, args.threads) < 1 or args.warmup < 0:
        parser.error("workload sizes must be positive and warmup nonnegative")
    args.baseline, args.candidate = args.baseline.resolve(strict=True), args.candidate.resolve(strict=True)
    records = []
    for run in range(args.runs):
        labels = ["baseline", "candidate"] if run % 2 == 0 else ["candidate", "baseline"]
        pair = {}
        for label in labels:
            print(f"Run {run + 1}: {label}", flush=True)
            pair[label] = benchmark(getattr(args, label), args)
            records.append({"run": run + 1, "variant": label, "cases": pair[label]})
        for case in pair["baseline"]:
            assert pair["baseline"][case]["responses_sha256"] == pair["candidate"][case]["responses_sha256"], f"response mismatch: {case}"
    summary = {}
    for case in records[0]["cases"]:
        values = {label: statistics.median(record["cases"][case]["p50_ms"] for record in records
                                          if record["variant"] == label) for label in ["baseline", "candidate"]}
        summary[case] = {**values, "speedup": values["baseline"] / values["candidate"]}
    report = {"platform": platform.platform(), "machine": platform.machine(),
              "python": platform.python_version(), "workload": {key: value for key, value in vars(args).items()
                                                               if not isinstance(value, Path)},
              "server": {"workers": 2, "max_blocking_threads_per_worker": 2,
                         "max_concurrent_database_tasks": 4, "concurrent_clients": 1},
              "binary_sha256": {label: hashlib.sha256(getattr(args, label).read_bytes()).hexdigest()
                                 for label in ["baseline", "candidate"]},
              "boundary": "keep-alive HTTP request/response; client JSON encoding and parsing excluded; memory storage; CPU compute",
              "records": records, "summary": summary}
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
