"""Shared chat request validation for the synchronous and async clients."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import Any


def _text(value: Any, name: str, limit: int) -> None:
    if (
        not isinstance(value, str)
        or not value.strip()
        or "\0" in value
        or len(value.encode("utf-8")) > limit
    ):
        raise ValueError(
            f"{name} must be nonempty text without NUL, at most {limit} UTF-8 bytes"
        )


def chat_body(
    text: str,
    *,
    history: Sequence[Mapping[str, Any]] | None,
    model: str,
    retrieval: Mapping[str, Any] | None,
    mode: str,
    context_mode: str,
    retrieval_query: str | None,
    answer_style: str,
    grounding: str,
    generation_timeout_ms: int,
    max_output_tokens: int,
) -> dict[str, Any]:
    # Both inputs need validation even when only retrieval_query is searched.
    _text(text, "text", 8191)
    if retrieval_query is not None:
        _text(retrieval_query, "retrieval_query", 8191)
        if context_mode == "conversation":
            raise ValueError(
                "retrieval_query cannot be combined with conversation context"
            )
    if (
        not isinstance(model, str)
        or not 1 <= len(model) <= 128
        or any(
            not (char.isascii() and (char.isalnum() or char in "._:-"))
            for char in model
        )
    ):
        raise ValueError(
            "model must be an ASCII model identifier of at most 128 characters"
        )

    body: dict[str, Any] = {"text": text}
    for name, value, choices, default in (
        ("mode", mode, ("answer", "retrieve"), "answer"),
        ("context_mode", context_mode, ("question", "conversation"), "question"),
        ("answer_style", answer_style, ("chat", "voice"), "chat"),
        ("grounding", grounding, ("standard", "strict"), "standard"),
    ):
        if value not in choices:
            raise ValueError(f"{name} must be one of {', '.join(choices)}")
        if value != default:
            body[name] = value
    for name, value, lower, upper, default in (
        ("generation_timeout_ms", generation_timeout_ms, 100, 60000, 60000),
        ("max_output_tokens", max_output_tokens, 128, 4096, 2048),
    ):
        if (
            isinstance(value, bool)
            or not isinstance(value, int)
            or not lower <= value <= upper
        ):
            raise ValueError(f"{name} must be an integer between {lower} and {upper}")
        if value != default:
            body[name] = value
    if model != "gpt-4.1-mini":
        body["model"] = model
    if retrieval_query is not None:
        body["retrieval_query"] = retrieval_query
    if history is not None:
        if (
            isinstance(history, (str, bytes))
            or not isinstance(history, Sequence)
            or len(history) > 20
        ):
            raise ValueError("history must contain at most 20 user/assistant messages")
        messages = []
        total_bytes = 0
        for message in history:
            if not isinstance(message, Mapping) or set(message) != {"role", "content"}:
                raise ValueError("history messages must contain only role and content")
            if message["role"] not in ("user", "assistant"):
                raise ValueError("history roles must be user or assistant")
            _text(message["content"], "history content", 8192)
            total_bytes += len(message["content"].encode("utf-8"))
            messages.append(dict(message))
        if total_bytes > 32768:
            raise ValueError("history must contain at most 32768 UTF-8 bytes in total")
        body["history"] = messages
    if retrieval is not None:
        if not isinstance(retrieval, Mapping) or any(
            not isinstance(key, str) for key in retrieval
        ):
            raise ValueError("retrieval must be a mapping of /retrieve options")
        if "text" in retrieval:
            raise ValueError(
                "put the question in text or retrieval_query, outside retrieval"
            )
        options = dict(retrieval)
        if "document_filters" in options:
            filters = options["document_filters"]
            if filters is None:
                options.pop("document_filters")
            else:
                if (
                    isinstance(filters, (str, bytes))
                    or not isinstance(filters, Sequence)
                    or any(not isinstance(item, Mapping) for item in filters)
                ):
                    raise ValueError(
                        "document_filters must be a sequence of filter mappings"
                    )
                options["document_filters"] = [dict(item) for item in filters]
        body["retrieval"] = options
    return body
