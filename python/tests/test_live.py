"""Opt-in integration against an isolated real server; no provider calls."""

import asyncio
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import unittest

import httpx

from vectors_sdk import APIError, AsyncClient, Client, QueryResult


@unittest.skipUnless(
    os.getenv("VECTORS_TEST_SERVER"),
    "set VECTORS_TEST_SERVER to the built server binary",
)
class LiveSDKTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory(prefix="vectors-python-sdk-")
        cls.addClassCleanup(cls.directory.cleanup)
        cls.shutdown = Path(cls.directory.name) / "shutdown"
        cls.log = open(Path(cls.directory.name) / "server.log", "w+")
        cls.addClassCleanup(cls.log.close)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        cls.url = f"http://127.0.0.1:{port}"
        # Isolate configuration/credentials from the user's real installation.
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("VECTORS_")
            and key not in ("OPENAI_API_KEY", "VOYAGE_API_KEY")
        }
        environment.update(
            VECTORS_API_TOKEN="sdk-test-token",
            VECTORS_COMPUTE_DEVICE="cpu",
            VECTORS_SHUTDOWN_FILE=str(cls.shutdown),
            VECTORS_WORKERS="2",
        )
        cls.process = subprocess.Popen(
            [
                str(Path(os.environ["VECTORS_TEST_SERVER"]).resolve()),
                "--bind",
                f"127.0.0.1:{port}",
                "--data-dir",
                str(Path(cls.directory.name) / "data"),
            ],
            env=environment,
            stdout=cls.log,
            stderr=subprocess.STDOUT,
        )
        cls.addClassCleanup(cls.stop_server)
        cls.client = Client(cls.url, token="sdk-test-token", max_retries=0)
        cls.addClassCleanup(cls.client.close)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if cls.process.poll() is not None:
                cls.log.seek(0)
                raise RuntimeError("server exited: " + cls.log.read())
            try:
                cls.client.readiness()
                break
            except Exception:
                time.sleep(0.05)
        else:
            raise RuntimeError("server readiness timed out")

    @classmethod
    def stop_server(cls):
        if cls.process.poll() is None:
            cls.shutdown.touch()
            try:
                cls.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                cls.process.kill()
                cls.process.wait(timeout=5)

    def test_sql_vector_import_auth_and_async_reads(self):
        db = self.client
        db.execute(
            "CREATE TABLE sdk_docs (id INTEGER PRIMARY KEY, text TEXT, tenant TEXT, embedding VECTOR(3)); CREATE INDEX sdk_tenant ON sdk_docs USING HASH (tenant)"
        )
        special = "O'Reilly '); DROP TABLE sdk_docs; -- 雪"
        db.execute(
            "INSERT INTO sdk_docs VALUES ($1,$2,$3,$4)", [0, special, "acme", [1, 0, 0]]
        )
        summary = db.insert(
            "sdk_docs",
            (
                {
                    "id": i,
                    "text": f"passage {i}",
                    "tenant": "acme",
                    "embedding": [1, i, 0],
                }
                for i in range(1, 24)
            ),
            batch_size=5,
        )
        self.assertEqual(
            (summary.input_rows, summary.rows_affected, summary.batches), (23, 23, 5)
        )
        result = db.execute(
            "SELECT id, text, embedding <=> $1 AS distance FROM sdk_docs WHERE tenant=$2 ORDER BY distance LIMIT $3",
            [[1, 0, 0], "acme", 3],
        )[0]
        self.assertIsInstance(result, QueryResult)
        self.assertEqual(result.rows[0], [0, special, 0.0])
        hits = db.search(
            "sdk_docs",
            [1, 0, 0],
            select=["id", "text"],
            filters=[{"column": "tenant", "operator": "eq", "value": "acme"}],
            limit=3,
        )
        self.assertEqual(
            [row[:2] for row in hits.rows], [row[:2] for row in result.rows]
        )
        intent = db.explain(
            "SELECT id FROM sdk_docs ORDER BY embedding <=> $1 LIMIT 3", [[1, 0, 0]]
        )
        self.assertTrue(intent["vector_search"]["optimized"])
        self.assertTrue(db.schema("sdk_docs"))
        self.assertTrue(db.indexes("sdk_docs"))
        self.assertTrue(db.settings()["authentication"])
        with Client(self.url) as anonymous:
            anonymous.health()
            with self.assertRaises(APIError) as raised:
                anonymous.tables()
            self.assertEqual(raised.exception.status_code, 401)

        async def read():
            async with AsyncClient(self.url, token="sdk-test-token") as client:
                queries = await asyncio.gather(
                    *(client.search("sdk_docs", [1, 0, 0], limit=2) for _ in range(4))
                )
                self.assertTrue(all(query.rows[0][0] == 0 for query in queries))
                return await client.execute(
                    "SELECT text FROM sdk_docs WHERE id=$1", [0]
                )

        self.assertEqual(asyncio.run(read())[0].rows, [[special]])

    def test_graph_citations_relationships_and_revision_conflicts(self):
        db = self.client
        # Configure only a non-secret profile; this does not call a provider.
        response = httpx.put(
            self.url + "/v1/settings/embeddings",
            headers={"Authorization": "Bearer sdk-test-token"},
            json={
                "provider": "openai",
                "model": "text-embedding-3-small",
                "dimensions": 3,
            },
            trust_env=False,
        )
        response.raise_for_status()
        info = db.create_collection("sdk_graph", semantic_neighbors=0)
        graph = db.collection("sdk_graph")
        self.assertEqual(graph.retrieve("recovery")["hits"], [])
        self.assertEqual(graph.search("recovery")["hits"], [])
        profile = json.dumps(info["config"]["profile"], separators=(",", ":"))
        # Seed precomputed vectors through public SQL/typed tables to avoid any
        # external provider. This fixture is not an ingestion shortcut example.
        text = "Żółć: committed records recover from WAL."
        for document_id in ("a/b?#雪", "second"):
            chunk_id = f"{len(document_id.encode())}:{document_id}:0"
            db.insert(
                info["tables"]["documents"],
                [
                    {
                        "document_id": document_id,
                        "title": "Guide",
                        "source": "docs.md",
                        "text": text,
                        "metadata": "{}",
                        "chunking": "{}",
                        "chunk_fingerprint": "",
                    }
                ],
            )
            db.insert(
                info["tables"]["chunks"],
                [
                    {
                        "chunk_id": chunk_id,
                        "document_id": document_id,
                        "ordinal": 0,
                        "start_byte": 0,
                        "end_byte": len(text.encode()),
                        "text": text,
                        "embedding_text": text,
                        "embedding_profile": profile,
                        "embedding": [1, 0, 0],
                    }
                ],
            )
        page = graph.browse(limit=1)
        self.assertEqual(page["total_nodes"], 2)
        source = graph.document("a/b?#雪")
        self.assertEqual(source["text"], text)
        ids = [node["chunk_id"] for node in graph.browse()["nodes"]]
        revision = graph.info()["revision"]
        edge = graph.upsert_relationship(
            ids[0], ids[1], "supports", weight=0.9, expected_revision=revision
        )
        with self.assertRaises(APIError) as raised:
            graph.delete_relationship(
                ids[0], ids[1], "supports", expected_revision=revision
            )
        self.assertEqual(raised.exception.code, "stale_revision")
        result = graph.neighborhood(ids[0], direction="outgoing", kind="supports")
        self.assertEqual(len(result["nodes"]), 2)
        for node in result["nodes"]:
            self.assertEqual(
                text.encode()[node["start_byte"] : node["end_byte"]].decode(),
                node["text"],
            )
        removed = graph.delete_relationship(
            ids[0], ids[1], "supports", expected_revision=edge["revision"]
        )
        graph.delete_document("a/b?#雪", expected_revision=removed["revision"])
        self.assertEqual(graph.info()["document_count"], 1)
        self.assertTrue(db.preview_chunks(text)["chunks"])
        # A nonempty collection requires embeddings, including lexical-weighted
        # retrieval. Surface its real error instead of silently falling back.
        with self.assertRaises(APIError):
            graph.retrieve("recovery", vector_weight=0)


if __name__ == "__main__":
    unittest.main()
