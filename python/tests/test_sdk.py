import asyncio
import inspect
import json
import unittest

import httpx

from vectors_sdk import (
    APIError,
    AsyncClient,
    AsyncCollection,
    BulkInsertError,
    Client,
    Collection,
    CommandResult,
    ProtocolError,
    QueryResult,
    TransportError,
)
from vectors_sdk._common import batches, encode, retry_after


def command(count=1):
    return {"results": [{"type": "command", "tag": "INSERT", "rows_affected": count}]}


def query():
    return {
        "type": "query",
        "columns": ["id", "text"],
        "schema": [
            {"name": "id", "data_type": "INTEGER"},
            {"name": "text", "data_type": "TEXT"},
        ],
        "rows": [[1, "雪"]],
        "row_count": 1,
        "rows_examined": 9,
    }


class SDKTests(unittest.TestCase):
    def test_sql_parameters_auth_path_prefix_and_typed_results(self):
        calls = []

        def handler(request):
            calls.append(request)
            return httpx.Response(
                200, json={"results": [query(), command()["results"][0]]}
            )

        with Client(
            "https://example.test/database/",
            token="secret",
            transport=httpx.MockTransport(handler),
        ) as client:
            results = client.execute("SELECT $1", ["' ; DROP TABLE x --"])
            self.assertIsInstance(results[0], QueryResult)
            self.assertIsInstance(results[1], CommandResult)
            self.assertEqual(results[0].to_dicts(), [{"id": 1, "text": "雪"}])
            self.assertEqual(results[0].rows_examined, 9)
        self.assertEqual(calls[0].url.path, "/database/v1/sql")
        self.assertEqual(calls[0].headers["Authorization"], "Bearer secret")
        self.assertEqual(
            json.loads(calls[0].content)["parameters"], ["' ; DROP TABLE x --"]
        )
        self.assertTrue(client._http.is_closed)

    def test_collection_columns_and_scalar_relationships_share_one_client(self):
        calls = []

        def handler(request):
            calls.append(request)
            return httpx.Response(200, json={"revision": 7, "relationships": []})

        columns = [{"name": "product_id", "data_type": "INTEGER", "nullable": False}]
        with Client(transport=httpx.MockTransport(handler)) as client:
            client.create_collection("manuals", document_columns=columns)
            revision = client.relationships()["revision"]
            client.create_relationship(
                "about_product",
                source_table="graph_manuals_documents",
                source_column="product_id",
                target_table="products",
                target_column="id",
                expected_revision=revision,
            )
            client.delete_relationship("about_product", expected_revision=8)
        self.assertEqual(json.loads(calls[0].content)["document_columns"], columns)
        self.assertEqual(calls[1].method, "GET")
        self.assertEqual(json.loads(calls[2].content)["expected_revision"], 7)
        self.assertEqual(calls[3].url.path, "/v1/relationships/about_product")
        self.assertEqual(json.loads(calls[3].content), {"expected_revision": 8})

    def test_named_relationship_writes_never_retry_overloads_or_stale_revisions(self):
        for status, code in ((503, "overloaded"), (409, "stale_revision")):
            with self.subTest(code=code):
                calls = []

                def handler(request):
                    calls.append(request)
                    return httpx.Response(
                        status,
                        headers={"Retry-After": "0"},
                        json={"error": {"code": code, "message": "retry manually"}},
                    )

                with Client(transport=httpx.MockTransport(handler)) as client:
                    for operation in (
                        lambda: client.create_relationship(
                            "manual_product",
                            source_table="graph_manuals_documents",
                            source_column="product_id",
                            target_table="products",
                            target_column="id",
                            expected_revision=7,
                        ),
                        lambda: client.delete_relationship(
                            "manual_product", expected_revision=7
                        ),
                    ):
                        before = len(calls)
                        with self.assertRaises(APIError) as raised:
                            operation()
                        self.assertEqual(raised.exception.code, code)
                        self.assertEqual(len(calls), before + 1)

    def test_insert_consumes_generator_lazily_and_reports_progress(self):
        consumed, requests = [], []

        def rows():
            for i in range(10):
                consumed.append(i)
                yield {"id": i, "text": "雪" * 5}

        def handler(request):
            requests.append(request)
            return httpx.Response(
                200, json=command(len(json.loads(request.content)["rows"]))
            )

        with Client(transport=httpx.MockTransport(handler)) as client:
            iterator = client.iter_insert("docs", rows(), batch_size=3)
            self.assertEqual(consumed, [])
            first = next(iterator)
            self.assertEqual((first.input_offset, first.row_count), (0, 3))
            self.assertEqual(len(consumed), 3)
            self.assertEqual([batch.row_count for batch in iterator], [3, 3, 1])
            self.assertEqual(len(requests), 4)
            empty = client.insert("docs", [])
            self.assertEqual(empty.input_rows, 0)
            self.assertEqual(len(requests), 4)

    def test_exact_byte_budget_includes_unicode_options_and_punctuation(self):
        rows = [{"text": "Żółć雪" * 3, "embedding": [0.1, 1.0]} for _ in range(7)]
        options = {"normalize_vectors": False, "on_conflict": "do_nothing"}
        cap = len(encode({"rows": rows[:2], **options}))
        encoded = list(
            batches(rows, batch_size=1000, max_batch_bytes=cap, options=options)
        )
        self.assertEqual([count for _, count in encoded], [2, 2, 2, 1])
        self.assertTrue(all(len(body) <= cap for body, _ in encoded))
        self.assertEqual(
            [row for body, _ in encoded for row in json.loads(body)["rows"]], rows
        )
        for bad in ([{"value": float("nan")}], [{1: "bad column"}], ["bad row"]):
            with self.assertRaises(ValueError):
                list(batches(bad, batch_size=1, max_batch_bytes=1000, options={}))
        with self.assertRaises(ValueError):
            list(batches(rows, batch_size=1, max_batch_bytes=2, options=options))

    def test_partial_failure_is_not_retried_and_exposes_acknowledged_offset(self):
        calls = []

        def handler(request):
            calls.append(request)
            if len(calls) == 1:
                return httpx.Response(200, json=command(1))
            return httpx.Response(
                503,
                json={"error": {"code": "overloaded", "message": "busy"}},
                headers={"Retry-After": "0"},
            )

        with Client(transport=httpx.MockTransport(handler)) as client:
            with self.assertRaises(BulkInsertError) as raised:
                client.insert("docs", ({"id": i} for i in range(6)), batch_size=2)
        error = raised.exception
        self.assertEqual(
            (error.input_offset, error.rows_affected, error.batches_completed),
            (2, 1, 1),
        )
        self.assertIsInstance(error.cause, APIError)
        self.assertEqual(len(calls), 2)

    def test_read_overload_retry_is_bounded_and_sql_and_provider_calls_never_retry(
        self,
    ):
        calls = []

        def handler(request):
            calls.append(request)
            return httpx.Response(
                503,
                headers={"Retry-After": "0"},
                json={"error": {"code": "overloaded", "message": "busy"}},
            )

        with Client(transport=httpx.MockTransport(handler), max_retries=2) as client:
            with self.assertRaises(APIError):
                client.search("docs", [1, 0])
            self.assertEqual(len(calls), 3)
            for operation in (
                lambda: client.execute("SELECT 1"),
                lambda: client.collection("docs").retrieve("question"),
                lambda: client.collection("docs").ingest("a", "text"),
            ):
                before = len(calls)
                with self.assertRaises(APIError):
                    operation()
                self.assertEqual(len(calls), before + 1)

    def test_http_and_protocol_errors_do_not_leak_html_or_token(self):
        for response, expected in [
            (httpx.Response(200, text="<html>secret</html>"), ProtocolError),
            (httpx.Response(502, text="<html>secret</html>"), APIError),
            (httpx.Response(401, json={"error": "secret"}), APIError),
        ]:
            with Client(
                token="secret", transport=httpx.MockTransport(lambda request: response)
            ) as client:
                with self.assertRaises(expected) as raised:
                    client.tables()
                self.assertNotIn("secret", str(raised.exception))

        def fail(request):
            raise httpx.ReadTimeout("secret", request=request)

        with Client(transport=httpx.MockTransport(fail)) as client:
            with self.assertRaises(TransportError) as raised:
                client.execute("INSERT INTO docs VALUES (1)")
            self.assertNotIn("secret", str(raised.exception))

    def test_graph_payloads_paths_and_revision_controls(self):
        calls = []

        def handler(request):
            calls.append(request)
            return httpx.Response(200, json={"revision": 9, "hits": []})

        with Client(transport=httpx.MockTransport(handler)) as client:
            graph = client.collection("knowledge")
            self.assertEqual(calls, [])
            graph.ingest(
                "a/b?#雪", "Text", metadata={"team": "rag"}, expected_revision=8
            )
            graph.document("a/b?#雪")
            self.assertIn(b"a%2Fb%3F%23%E9%9B%AA", calls[-1].url.raw_path)
            graph.delete_document("a/b?#雪", expected_revision=9)
            self.assertEqual(json.loads(calls[-1].content), {"expected_revision": 9})
            graph.neighborhood(
                "1:a:0", direction="incoming", kind="supports", min_weight=0.4
            )
            self.assertEqual(calls[-1].url.params["direction"], "incoming")
            self.assertEqual(calls[-1].url.params["kind"], "supports")
            graph.retrieve("WAL recovery", vector_weight=0, max_context_bytes=1024)
            self.assertEqual(json.loads(calls[-1].content)["vector_weight"], 0)
            graph.upsert_relationship(
                "a", "b", "supports", weight=0.9, expected_revision=9
            )
            graph.delete_relationship("a", "b", "supports", expected_revision=10)
            self.assertEqual(calls[-1].method, "DELETE")
            self.assertEqual(json.loads(calls[-1].content)["expected_revision"], 10)

    def test_retrieval_controls_preserve_legacy_defaults_and_evidence_fields(self):
        payloads = []
        evidence = {
            "seed_chunk_id": "seed",
            "edges": [
                {
                    "from_chunk": "answer",
                    "to_chunk": "seed",
                    "kind": "supports",
                    "weight": 0.8,
                }
            ],
        }

        def handler(request):
            self.assertEqual(request.url.path, "/v1/graph/collections/docs/retrieve")
            payloads.append(json.loads(request.content))
            return httpx.Response(200, json={"hits": [{"retrieval_path": evidence}]})

        with Client(transport=httpx.MockTransport(handler)) as client:
            graph = client.collection("docs")
            graph.retrieve("question")
            graph.retrieve(
                "question",
                direction="outgoing",
                kind=None,
                min_weight=0,
                document_filters=None,
            )
            result = graph.retrieve(
                "question", direction="incoming", kind="supports", min_weight=0.8
            )
        self.assertEqual(payloads[0], payloads[1])
        for field in ("direction", "kind", "min_weight", "document_filters"):
            self.assertNotIn(field, payloads[0])
        self.assertEqual(
            payloads[2],
            {
                **payloads[0],
                "direction": "incoming",
                "kind": "supports",
                "min_weight": 0.8,
            },
        )
        self.assertEqual(result["hits"][0]["retrieval_path"], evidence)

    def test_retrieval_document_filters_preserve_types_and_explicit_empty_list(self):
        payloads = []

        def handler(request):
            payloads.append(json.loads(request.content))
            return httpx.Response(200, json={"hits": []})

        filters = [
            {"column": "product_id", "operator": "eq", "value": 7},
            {"column": "published", "operator": "eq", "value": False},
            {"column": "category", "operator": "ne", "value": "雪"},
            {"column": "weight", "operator": "gte", "value": 0.5},
            {"column": "retired_at", "operator": "eq", "value": None},
        ]
        with Client(transport=httpx.MockTransport(handler)) as client:
            graph = client.collection("manuals")
            graph.retrieve("question", document_filters=[])
            graph.retrieve("question", document_filters=filters)
            before = len(payloads)
            with self.assertRaises(ValueError):
                graph.retrieve(
                    "question",
                    document_filters=[
                        {"column": "weight", "operator": "eq", "value": float("nan")}
                    ],
                )
            self.assertEqual(len(payloads), before)
        self.assertEqual(payloads[0]["document_filters"], [])
        self.assertEqual(payloads[1]["document_filters"], filters)

    def test_redirects_are_not_followed_and_configuration_is_validated(self):
        calls = []

        def handler(request):
            calls.append(request)
            return httpx.Response(307, headers={"Location": "https://other.test"})

        with Client(token="secret", transport=httpx.MockTransport(handler)) as client:
            with self.assertRaises(APIError):
                client.tables()
        self.assertEqual(len(calls), 1)
        for url in (
            "ftp://example.test",
            "https://user:secret@example.test",
            "https://example.test?token=secret",
        ):
            with self.assertRaises(ValueError):
                Client(url)
        for retries in (-1, 11, True):
            with self.assertRaises(ValueError):
                Client(max_retries=retries)
        self.assertIsNone(retry_after("nan"))
        self.assertIsNone(retry_after("invalid"))

    def test_duplicate_projection_labels_are_not_silently_lost(self):
        result = QueryResult(["id", "id"], [], [[1, 2]], 1, 1)
        with self.assertRaises(ValueError):
            result.to_dicts()

    def test_invalid_result_shapes_raise_protocol_errors(self):
        invalid = [
            None,
            {},
            {"type": "command", "tag": "INSERT", "rows_affected": "1"},
            {**query(), "rows": ["ab"]},
            {**query(), "row_count": True},
            {**query(), "schema": []},
            {**query(), "rows_examined": -1},
        ]
        for value in invalid:
            with Client(
                transport=httpx.MockTransport(
                    lambda request: httpx.Response(200, json={"results": [value]})
                )
            ) as client:
                with self.assertRaises(ProtocolError):
                    client.execute("SELECT 1")


class AsyncSDKTests(unittest.IsolatedAsyncioTestCase):
    async def test_async_retrieval_controls_match_sync_payloads(self):
        sync_payloads, async_payloads = [], []

        def sync_handler(request):
            sync_payloads.append(json.loads(request.content))
            return httpx.Response(200, json={"hits": []})

        async def async_handler(request):
            async_payloads.append(json.loads(request.content))
            return httpx.Response(200, json={"hits": []})

        for options in (
            {},
            {"direction": "both", "kind": "references", "min_weight": 0.4},
            {"document_filters": None},
            {"document_filters": []},
            {
                "document_filters": [
                    {"column": "product_id", "operator": "eq", "value": 7},
                    {"column": "published", "operator": "eq", "value": False},
                    {"column": "nullable", "operator": "ne", "value": None},
                ]
            },
        ):
            with Client(transport=httpx.MockTransport(sync_handler)) as client:
                client.collection("docs").retrieve("question", **options)
            async with AsyncClient(
                transport=httpx.MockTransport(async_handler)
            ) as client:
                await client.collection("docs").retrieve("question", **options)
        self.assertEqual(async_payloads, sync_payloads)

    async def test_async_named_relationships_preserve_revision_and_paths(self):
        requests = []

        async def handler(request):
            requests.append(request)
            return httpx.Response(200, json={"revision": 7, "relationships": []})

        async with AsyncClient(
            "https://example.test/database/", transport=httpx.MockTransport(handler)
        ) as client:
            listed = await client.relationships()
            await client.create_relationship(
                "manual_product",
                source_table="graph_manuals_documents",
                source_column="product_id",
                target_table="products",
                target_column="id",
                expected_revision=listed["revision"],
            )
            await client.delete_relationship("manual_product", expected_revision=8)
        self.assertEqual(
            [(request.method, request.url.path) for request in requests],
            [
                ("GET", "/database/v1/relationships"),
                ("POST", "/database/v1/relationships"),
                ("DELETE", "/database/v1/relationships/manual_product"),
            ],
        )
        self.assertEqual(
            json.loads(requests[1].content),
            {
                "name": "manual_product",
                "source_table": "graph_manuals_documents",
                "source_column": "product_id",
                "target_table": "products",
                "target_column": "id",
                "expected_revision": 7,
            },
        )
        self.assertEqual(json.loads(requests[2].content), {"expected_revision": 8})

    async def test_async_ingestion_search_and_graph(self):
        requests = []

        async def handler(request):
            await asyncio.sleep(0)
            requests.append(request)
            if request.url.path.endswith("/rows"):
                return httpx.Response(
                    200, json=command(len(json.loads(request.content)["rows"]))
                )
            if request.url.path.endswith("/sql"):
                return httpx.Response(200, json={"results": [query()]})
            if request.url.path.endswith("/search"):
                return httpx.Response(200, json=query())
            return httpx.Response(200, json={"hits": [], "revision": 1})

        async with AsyncClient(transport=httpx.MockTransport(handler)) as client:
            summary = await client.insert(
                "docs", ({"id": i} for i in range(5)), batch_size=2
            )
            self.assertEqual((summary.input_rows, summary.batches), (5, 3))
            results = await asyncio.gather(
                *(client.search("docs", [1.0, 0.0]) for _ in range(4))
            )
            self.assertTrue(all(result.rows_examined == 9 for result in results))
            self.assertEqual((await client.execute("SELECT $1", [1]))[0].rows[0][0], 1)
            self.assertEqual(
                (await client.collection("docs").retrieve("WAL", vector_weight=0))[
                    "hits"
                ],
                [],
            )
            await client.collection("docs").neighborhood("1:a:0")
        self.assertTrue(client._http.is_closed)

    async def test_async_cancellation_propagates_without_retry(self):
        started = asyncio.Event()

        async def handler(request):
            started.set()
            await asyncio.Event().wait()

        async with AsyncClient(transport=httpx.MockTransport(handler)) as client:
            task = asyncio.create_task(client.insert("docs", [{"id": 1}]))
            await started.wait()
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task

    async def test_async_read_retries_and_bulk_failure_progress(self):
        calls = []

        async def handler(request):
            calls.append(request)
            return httpx.Response(
                503,
                headers={"Retry-After": "0"},
                json={"error": {"code": "overloaded", "message": "busy"}},
            )

        async with AsyncClient(
            transport=httpx.MockTransport(handler), max_retries=1
        ) as client:
            with self.assertRaises(APIError):
                await client.tables()
            self.assertEqual(len(calls), 2)
            with self.assertRaises(BulkInsertError) as raised:
                await client.insert("docs", [{"id": 1}])
            self.assertEqual(raised.exception.input_offset, 0)
            self.assertEqual(len(calls), 3)

    async def test_sync_async_public_method_signatures_match(self):
        for sync, asynchronous in (
            (Client, AsyncClient),
            (Collection, AsyncCollection),
        ):
            for name, method in inspect.getmembers(sync, inspect.isfunction):
                if name.startswith("_") or name == "close":
                    continue
                other = getattr(asynchronous, name)
                left = inspect.signature(method).parameters
                right = inspect.signature(other).parameters
                self.assertEqual(list(left), list(right), name)
                self.assertEqual(
                    [p.default for p in left.values()],
                    [p.default for p in right.values()],
                    name,
                )


if __name__ == "__main__":
    unittest.main()
