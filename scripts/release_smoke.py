#!/usr/bin/env python3
r"""Smoke-test an extracted release binary using Python 3.10+ and no provider keys.

Linux/macOS: python3 scripts/release_smoke.py --server ./vectors-server --expected-version v0.8.0
Windows:     python scripts/release_smoke.py --server .\vectors-server.exe --expected-version v0.8.0

Only loopback HTTP and a temporary durable database are used. This checks the
embedded UI assets, not browser rendering. A nonzero exit blocks publication.
"""

import argparse
import contextlib
import http.client
import json
import math
import os
from pathlib import Path
import re
import secrets
import socket
import subprocess
import sys
import tempfile
import time


class SmokeError(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise SmokeError(message)


class API:
    def __init__(self, port, token):
        self.port = port
        self.token = token

    def request(self, path, payload=None, *, status=200, authenticated=True, timeout=5):
        headers = {"Accept-Encoding": "identity"}
        if authenticated:
            headers["Authorization"] = f"Bearer {self.token}"
        body = None
        if payload is not None:
            body = json.dumps(payload, allow_nan=False).encode("utf-8")
            headers["Content-Type"] = "application/json"
        # HTTPConnection talks directly to loopback and ignores proxy settings.
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)
        try:
            connection.request("POST" if body is not None else "GET", path, body, headers)
            response = connection.getresponse()
            data = response.read(4 * 1024 * 1024 + 1)
            require(len(data) <= 4 * 1024 * 1024, f"{path}: response exceeded 4 MiB")
            require(response.status == status,
                    f"{path}: expected HTTP {status}, got {response.status}: {data[:500]!r}")
            content_type = response.getheader("Content-Type", "")
            if "application/json" in content_type:
                return json.loads(data)
            return data.decode("utf-8")
        finally:
            connection.close()


@contextlib.contextmanager
def running_server(binary, directory, timeout, expected_version):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    token = secrets.token_hex(32)
    shutdown = directory / "shutdown.request"
    shutdown.unlink(missing_ok=True)
    environment = {
        key: value for key, value in os.environ.items()
        if not key.upper().startswith("VECTORS_")
        and key.upper() not in {"OPENAI_API_KEY", "VOYAGE_API_KEY"}
    }
    environment.update({
        "VECTORS_API_TOKEN": token,
        "VECTORS_SHUTDOWN_FILE": str(shutdown),
        "VECTORS_HTTP_WORKERS": "1",
        "VECTORS_HTTP_MAX_BLOCKING_THREADS_PER_WORKER": "2",
        "VECTORS_MAX_CONCURRENT_DATABASE_TASKS": "2",
        "VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS": "2",
        "RAYON_NUM_THREADS": "2",
    })
    with tempfile.TemporaryFile() as log:
        process = subprocess.Popen(
            [str(binary), "--port", str(port), "--compute", "cpu",
             "--data-dir", str(directory / "data")],
            cwd=directory, env=environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
        )
        failed = False
        forced = False
        try:
            api = API(port, token)
            deadline = time.monotonic() + timeout
            while True:
                require(process.poll() is None, "server exited before becoming ready")
                try:
                    health = api.request("/healthz", authenticated=False, timeout=1)
                    break
                except (OSError, http.client.HTTPException):
                    require(time.monotonic() < deadline, "server startup timed out")
                    time.sleep(0.05)
            require(health.get("status") == "ok", "health did not report ok")
            require(health.get("version") == expected_version, "health version differs from release")
            require(health.get("storage") == "durable", "server is not using temporary durable storage")
            # Verify this is our authenticated instance before any writes, even
            # if another process happened to claim the selected ephemeral port.
            settings = api.request("/v1/settings/server")
            require(settings.get("authentication") is True, "temporary API authentication is missing")
            api.request("/v1/tables", authenticated=False, status=401)
            yield api
        except BaseException:
            failed = True
            raise
        finally:
            if process.poll() is None:
                try:
                    shutdown.touch()
                    process.wait(timeout=30)
                except (OSError, subprocess.TimeoutExpired):
                    forced = True
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
            if failed or forced or process.returncode != 0:
                log.seek(0)
                print("Temporary server log:\n" + log.read().decode("utf-8", errors="replace")[-16000:],
                      file=sys.stderr)
            if not failed:
                require(not forced and process.returncode == 0,
                        "server did not shut down cleanly using its shutdown file")


def check_assets(api):
    for path, markers in {
        "/": ["Connections", 'id="view-connections"', 'id="graph-canvas"',
              'id="graph-search-form"', 'id="graph-seeds"',
              'id="graph-retrieval-direction"', 'id="graph-retrieval-kind"',
              'id="graph-retrieval-min-weight"', 'id="graph-document-fields"',
              'id="graph-filters-panel"', 'id="graph-document-filters"',
              'id="relationship-dialog"'],
        "/assets/app.js": ["retrieval_path", "graph-retrieval-direction", "/retrieve",
                           "document_columns", "document_filters", "/relationships"],
        "/assets/app.css": [".graph-workspace", ".graph-retrieval-path", ".relationship-card"],
    }.items():
        asset = api.request(path, authenticated=False)
        require(isinstance(asset, str), f"{path}: expected a text asset")
        for marker in markers:
            require(marker in asset, f"{path}: embedded UI is missing {marker!r}; rebuild the release")


def sql(api, statement):
    return api.request("/v1/sql", {"sql": statement})["results"]


def check_apis(api):
    require(api.request("/v1/tables")["tables"] == [], "temporary database was not empty")
    sql(api, "CREATE TABLE release_smoke (id INTEGER PRIMARY KEY, title TEXT, embedding VECTOR(3))")
    inserted = api.request("/v1/tables/release_smoke/rows", {"rows": [
        {"id": 1, "title": "Release ✓", "embedding": [1, 0, 0]},
        {"id": 2, "title": "Other", "embedding": [0, 1, 0]},
    ]})
    require(inserted["results"][0]["rows_affected"] == 2, "typed insertion did not store both rows")
    result = api.request("/v1/vector/search", {
        "table": "release_smoke", "vector_column": "embedding", "query": [1, 0, 0],
        "metric": "cosine", "select": ["id", "title"], "limit": 1,
    })
    require(result["columns"] == ["id", "title", "distance"], "vector result schema is incorrect")
    require(result["rows"] == [[1, "Release ✓", 0.0]], "vector search returned the wrong nearest row")

    source = "# Release\n\nUnicode citation: café, 東京.\n\nA second paragraph."
    preview = api.request("/v1/graph/chunk", {"text": source, "chunking": {
        "max_characters": 32, "overlap_characters": 4, "max_chunks": 16,
    }})
    require(bool(preview["chunks"]), "graph chunk preview returned no passages")
    source_bytes = source.encode("utf-8")
    covered = 0
    for chunk in preview["chunks"]:
        start, end = chunk["byte_start"], chunk["byte_end"]
        require(0 <= start <= covered < end <= len(source_bytes), "chunk coverage or progress is invalid")
        require(source_bytes[start:end].decode("utf-8") == chunk["text"], "chunk citation bytes do not match")
        covered = end
    require(covered == len(source_bytes), "chunk preview omitted the end of the source")

    created = api.request("/v1/graph/collections", {
        "name": "release_smoke", "semantic_neighbors": 0,
        "document_columns": [{"name": "record_id", "data_type": "INTEGER", "nullable": False}],
    })
    require(created["config"]["name"] == "release_smoke", "graph collection creation failed")
    require(created["document_columns"][0]["name"] == "record_id", "document schema was not created")
    browse = api.request("/v1/graph/collections/release_smoke/graph?limit=2")
    require(browse["nodes"] == [] and browse["edges"] == [], "new graph collection was not empty")
    retrieved = api.request("/v1/graph/collections/release_smoke/retrieve", {
        "text": "Release check", "direction": "incoming", "kind": "supports", "min_weight": 0.5,
        "document_filters": [{"column": "record_id", "operator": "eq", "value": 1}],
    })
    require(retrieved["hits"] == [] and retrieved["embedding_usage"]["total_tokens"] == 0,
            "empty graph retrieval should succeed without provider usage")
    api.request("/v1/graph/collections/release_smoke/retrieve",
                {"text": "Release check", "min_weight": 2}, status=400)
    for column, value, code in [("missing", 1, "unknown_column"),
                                ("record_id", "1", "invalid_value")]:
        rejected = api.request("/v1/graph/collections/release_smoke/retrieve", {
            "text": "Release check",
            "document_filters": [{"column": column, "operator": "eq", "value": value}],
        }, status=400)
        require(rejected["error"]["code"] == code,
                "invalid document filter was not rejected before embedding")

    # Synthetic source-only fixture: no embeddings or provider calls.
    api.request("/v1/tables/graph_release_smoke_documents/rows", {"rows": [{
        "document_id": "manual", "title": "Manual", "source": "fixture", "text": "Release",
        "metadata": "{}", "chunking": "{}", "chunk_fingerprint": "", "record_id": 1,
    }]})
    revision = api.request("/v1/relationships")["revision"]
    relationship = api.request("/v1/relationships", {
        "name": "release_record", "source_table": "graph_release_smoke_documents",
        "source_column": "record_id", "target_table": "release_smoke", "target_column": "id",
        "expected_revision": revision,
    })
    require(relationship["relationship"]["valid"], "cross-table relationship was not saved")
    sql(api, "CREATE TABLE release_labels (record_id INTEGER UNIQUE, label TEXT); "
             "INSERT INTO release_labels VALUES (1, 'Published')")
    check_relationship_join(api)


def check_relationship_join(api):
    links = api.request("/v1/relationships")
    require(len(links["relationships"]) == 1 and links["relationships"][0]["valid"],
            "relationship catalog is incomplete or invalid")
    document = api.request("/v1/graph/collections/release_smoke/documents/manual")
    require(document["metadata"]["record_id"] == 1, "typed document metadata was lost")
    result = sql(api, "SELECT d.document_id,r.title,l.label,cosine_distance(r.embedding,ARRAY[1,0,0]) AS distance "
                 "FROM graph_release_smoke_documents d LEFT JOIN release_smoke r ON d.record_id=r.id "
                 "JOIN release_labels l ON r.id=l.record_id "
                 "ORDER BY distance LIMIT 1")[0]
    require(result["rows"] == [["manual", "Release ✓", "Published", 0.0]],
            "vector-ranked three-table SQL join returned wrong data")


def run(binary, expected_version, timeout):
    version = subprocess.run([str(binary), "--version"], capture_output=True, text=True,
                             check=True, timeout=timeout).stdout.strip()
    require(version == f"vectors-server {expected_version}",
            f"binary version mismatch: expected vectors-server {expected_version}, got {version!r}")
    with tempfile.TemporaryDirectory(prefix="vectors-release-smoke-") as temporary:
        directory = Path(temporary)
        with running_server(binary, directory, timeout, expected_version) as api:
            check_assets(api)
            check_apis(api)
        with running_server(binary, directory, timeout, expected_version) as api:
            result = sql(api, "SELECT id, title FROM release_smoke ORDER BY id")[0]
            require(result["rows"] == [[1, "Release ✓"], [2, "Other"]], "SQL data did not survive restart")
            collection = api.request("/v1/graph/collections/release_smoke")
            require(collection["config"]["name"] == "release_smoke" and collection["chunk_count"] == 0,
                    "graph collection did not survive restart")
            check_relationship_join(api)
    print(f"PASS vectors-server {expected_version}: embedded UI, authenticated SQL/vector/GraphRAG APIs, typed filters, relationships, join chains, restart")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--server", required=True, type=Path, help="extracted vectors-server binary")
    parser.add_argument("--expected-version", required=True, help="release version or tag, e.g. v0.8.0")
    parser.add_argument("--timeout", type=float, default=60, help="startup/version timeout in seconds (default: 60)")
    args = parser.parse_args()
    version = args.expected_version.removeprefix("v")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version):
        parser.error("--expected-version must be a semantic version or a v-prefixed release tag")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be finite and positive")
    binary = args.server.resolve()
    if not binary.is_file():
        parser.error(f"server binary does not exist: {binary}")
    try:
        run(binary, version, args.timeout)
    except (SmokeError, OSError, ValueError, KeyError, TypeError,
            subprocess.SubprocessError, http.client.HTTPException) as error:
        print(f"FAIL release smoke: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
