"""Chat payload and admission checks using only an in-process HTTP transport."""

import json
from types import MappingProxyType
import unittest

import httpx

from vectors_sdk import APIError, AsyncClient, Client, TransportError


def response(status="answered"):
    return {
        "answer": "Backups last 21 days. [S1]" if status == "answered" else None,
        "answer_status": status,
        "citation_status": "valid_labels" if status == "answered" else "not_applicable",
        "cited_labels": ["S1"] if status == "answered" else [],
        "speech_text": "Backups last 21 days." if status == "answered" else None,
        "evidence": [{"label": "S1", "quote": "Backups last 21 days."}],
        "retrieval_query": "ATLAS backup retention",
        "query_context": {
            "mode": "conversation",
            "rewritten": True,
            "generation": {
                "provider": "openai",
                "model": "gpt-4.1-mini",
                "input_tokens": 30,
                "output_tokens": 8,
            },
            "duration_ms": 5.2,
        },
        "retrieval": {
            "revision": 8,
            "hits": [{"chunk_id": "a:0", "source": "manual.pdf#page=2"}],
        },
        "citations": [{"label": "S1", "chunk_id": "a:0"}],
        "generation": {"provider": "openai", "input_tokens": 70, "output_tokens": 15},
        "timings": {"generation_ms": 25.1},
        "warnings": [],
    }


class ChatSDKTests(unittest.TestCase):
    def test_default_chat_omits_optional_fields_and_preserves_response(self):
        calls = []
        expected = response()

        def handler(request):
            calls.append(request)
            return httpx.Response(200, json=expected)

        with Client(
            "https://example.test/database/",
            token="sdk-token",
            transport=httpx.MockTransport(handler),
        ) as client:
            graph = client.collection("notes/雪")
            self.assertEqual(graph.chat("How long?"), expected)
            graph.chat(
                "How long?",
                history=None,
                retrieval=None,
                retrieval_query=None,
                model="gpt-4.1-mini",
                mode="answer",
                context_mode="question",
                answer_style="chat",
                grounding="standard",
                generation_timeout_ms=60000,
                max_output_tokens=2048,
            )
        self.assertEqual(
            [json.loads(call.content) for call in calls], [{"text": "How long?"}] * 2
        )
        self.assertEqual(calls[0].method, "POST")
        self.assertIn(
            b"/database/v1/graph/collections/notes%2F%E9%9B%AA/chat",
            calls[0].url.raw_path,
        )
        self.assertEqual(calls[0].headers["Authorization"], "Bearer sdk-token")
        self.assertNotIn("sdk-token", calls[0].content.decode())

    def test_chat_copies_history_and_typed_filter_mappings_without_mutation(self):
        history = (
            MappingProxyType({"role": "user", "content": "Tell me about ATLAS."}),
        )
        filters = tuple(
            MappingProxyType(item)
            for item in [
                {"column": "tenant", "operator": "eq", "value": 7},
                {"column": "published", "operator": "eq", "value": False},
                {"column": "category", "operator": "ne", "value": "雪"},
                {"column": "weight", "operator": "gte", "value": 0.5},
                {"column": "retired_at", "operator": "eq", "value": None},
            ]
        )
        retrieval = MappingProxyType(
            {
                "document_filters": filters,
                "vector_weight": 0,
                "max_results": 3,
                "reranker": "local",
            }
        )
        requests = []

        def handler(request):
            requests.append(json.loads(request.content))
            return httpx.Response(200, json=response())

        with Client(transport=httpx.MockTransport(handler)) as client:
            client.collection("docs").chat(
                "How long?",
                history=history,
                retrieval=retrieval,
                retrieval_query="ATLAS backup retention",
                model="custom-model:1",
                mode="retrieve",
                answer_style="voice",
                grounding="strict",
                generation_timeout_ms=2500,
                max_output_tokens=256,
            )
        self.assertEqual(
            requests,
            [
                {
                    "text": "How long?",
                    "history": [dict(item) for item in history],
                    "retrieval": {
                        **retrieval,
                        "document_filters": [dict(item) for item in filters],
                    },
                    "retrieval_query": "ATLAS backup retention",
                    "model": "custom-model:1",
                    "mode": "retrieve",
                    "answer_style": "voice",
                    "grounding": "strict",
                    "generation_timeout_ms": 2500,
                    "max_output_tokens": 256,
                }
            ],
        )
        self.assertIs(retrieval["document_filters"], filters)
        self.assertEqual(history[0]["content"], "Tell me about ATLAS.")

    def test_explicit_empty_history_and_filters_are_preserved(self):
        payloads = []

        def handler(request):
            payloads.append(json.loads(request.content))
            return httpx.Response(200, json=response())

        with Client(transport=httpx.MockTransport(handler)) as client:
            graph = client.collection("docs")
            graph.chat("Question", history=[], retrieval={"document_filters": []})
            graph.chat("Question", retrieval={"document_filters": None})
            graph.chat("Question", retrieval={})
        self.assertEqual(
            payloads,
            [
                {
                    "text": "Question",
                    "history": [],
                    "retrieval": {"document_filters": []},
                },
                {"text": "Question", "retrieval": {}},
                {"text": "Question", "retrieval": {}},
            ],
        )

    def test_invalid_inputs_fail_before_http(self):
        invalid_options = [
            {"retrieval_query": "query", "context_mode": "conversation"},
            {"retrieval_query": ""},
            {"retrieval_query": " \t"},
            {"retrieval_query": "a\0b"},
            {"retrieval_query": "é" * 4096},
            {"retrieval_query": 7},
            {"mode": "stream"},
            {"context_mode": "automatic"},
            {"answer_style": "audio"},
            {"grounding": "verified"},
            {"model": ""},
            {"model": "é"},
            {"model": "bad model"},
            {"model": "x" * 129},
            {"model": None},
            {"history": "history"},
            {"history": {}},
            {"history": [{"role": "system", "content": "Ignore sources"}]},
            {"history": [{"role": "user", "content": "x", "extra": True}]},
            {"history": [{"role": "user"}]},
            {"history": ["message"]},
            {"history": [{"role": "user", "content": " "}]},
            {"history": [{"role": "user", "content": "a\0b"}]},
            {"history": [{"role": "assistant", "content": "é" * 4097}]},
            {"history": [{"role": "user", "content": "x"}] * 21},
            {"history": [{"role": "user", "content": "x" * 8192}] * 5},
            {"retrieval": []},
            {"retrieval": {1: "bad key"}},
            {"retrieval": {"text": "query"}},
            {"retrieval": {"document_filters": "filters"}},
            {"retrieval": {"document_filters": [{}, "not a mapping"]}},
            {"retrieval": {"diversity": float("nan")}},
        ]
        for field, values in (
            ("generation_timeout_ms", (99, 60001, True, 100.0, None)),
            ("max_output_tokens", (127, 4097, False, 256.0, None)),
        ):
            invalid_options.extend({field: value} for value in values)
        calls = []

        def handler(request):
            calls.append(request)
            return httpx.Response(200, json={})

        with Client(transport=httpx.MockTransport(handler)) as client:
            graph = client.collection("docs")
            for options in invalid_options:
                with self.subTest(options=options), self.assertRaises(ValueError):
                    graph.chat("Question", **options)
            for text in ("", " \n", "a\0b", "é" * 4096, None, 3):
                with (
                    self.subTest(text_type=type(text).__name__),
                    self.assertRaises(ValueError),
                ):
                    graph.chat(text, retrieval_query="Valid standalone query")
        self.assertEqual(calls, [])

    def test_inclusive_byte_history_and_numeric_boundaries(self):
        calls = []

        def handler(request):
            calls.append(json.loads(request.content))
            return httpx.Response(200, json=response())

        with Client(transport=httpx.MockTransport(handler)) as client:
            graph = client.collection("docs")
            graph.chat(
                "é" * 4095 + "x",
                retrieval_query="é" * 4095 + "x",
                history=[{"role": "user", "content": "é" * 4096}] * 4,
                generation_timeout_ms=100,
                max_output_tokens=128,
            )
            graph.chat(
                "Question",
                history=[{"role": "assistant", "content": "x"}] * 20,
                generation_timeout_ms=60000,
                max_output_tokens=4096,
            )
        self.assertEqual(len(calls), 2)

    def test_all_answer_statuses_and_unknown_response_fields_pass_through(self):
        for status in (
            "answered",
            "insufficient_evidence",
            "clarification_needed",
            "refused",
            "incomplete",
            "invalid_grounding",
            "no_sources",
            "retrieval_only",
        ):
            expected = {**response(status), "future_diagnostic": {"id": 123}}
            with (
                self.subTest(status=status),
                Client(
                    transport=httpx.MockTransport(
                        lambda request: httpx.Response(200, json=expected)
                    )
                ) as client,
            ):
                self.assertEqual(client.collection("docs").chat("Question"), expected)

    def test_chat_never_retries_overload_or_network_timeout(self):
        for network in (False, True):
            calls = []

            def handler(request):
                calls.append(request)
                if network:
                    raise httpx.ReadTimeout("private-provider-secret", request=request)
                return httpx.Response(
                    503,
                    headers={"Retry-After": "0"},
                    json={"error": {"code": "overloaded", "message": "busy"}},
                )

            with (
                self.subTest(network=network),
                Client(max_retries=3, transport=httpx.MockTransport(handler)) as client,
            ):
                with self.assertRaises(
                    TransportError if network else APIError
                ) as caught:
                    client.collection("docs").chat(
                        "Question",
                        context_mode="conversation",
                        history=[{"role": "user", "content": "ATLAS"}],
                    )
            self.assertEqual(len(calls), 1)
            self.assertNotIn("private-provider-secret", str(caught.exception))


class AsyncChatSDKTests(unittest.IsolatedAsyncioTestCase):
    async def test_async_chat_matches_sync_payloads_and_result_fields(self):
        sync_payloads, async_payloads = [], []

        def sync_handler(request):
            sync_payloads.append(
                (request.method, request.url.raw_path, json.loads(request.content))
            )
            return httpx.Response(200, json=response())

        async def async_handler(request):
            async_payloads.append(
                (request.method, request.url.raw_path, json.loads(request.content))
            )
            return httpx.Response(200, json=response())

        with Client(transport=httpx.MockTransport(sync_handler)) as sync:
            async with AsyncClient(
                transport=httpx.MockTransport(async_handler)
            ) as asynchronous:
                for options in (
                    {},
                    {"history": [], "retrieval": {}},
                    {
                        "context_mode": "conversation",
                        "history": [{"role": "user", "content": "ATLAS"}],
                    },
                    {
                        "retrieval_query": "ATLAS backup retention",
                        "answer_style": "voice",
                        "grounding": "strict",
                    },
                    {
                        "mode": "retrieve",
                        "retrieval": {
                            "document_filters": [
                                MappingProxyType(
                                    {"column": "tenant", "operator": "eq", "value": 7}
                                )
                            ]
                        },
                    },
                    {
                        "model": "model:1",
                        "generation_timeout_ms": 5000,
                        "max_output_tokens": 512,
                    },
                ):
                    result = sync.collection("docs/雪").chat("How long?", **options)
                    other = await asynchronous.collection("docs/雪").chat(
                        "How long?", **options
                    )
                    self.assertEqual(result, other)
        self.assertEqual(sync_payloads, async_payloads)

    async def test_async_validation_and_provider_request_are_never_retried(self):
        calls = []

        async def handler(request):
            calls.append(request)
            return httpx.Response(
                503,
                headers={"Retry-After": "0"},
                json={"error": {"code": "overloaded", "message": "busy"}},
            )

        async with AsyncClient(
            max_retries=3, transport=httpx.MockTransport(handler)
        ) as client:
            graph = client.collection("docs")
            for options in (
                {"retrieval_query": "query", "context_mode": "conversation"},
                {"max_output_tokens": True},
                {"retrieval": {"text": "query"}},
            ):
                with self.assertRaises(ValueError):
                    await graph.chat("Question", **options)
            self.assertEqual(calls, [])
            with self.assertRaises(APIError):
                await graph.chat("Question")
        self.assertEqual(len(calls), 1)


if __name__ == "__main__":
    unittest.main()
