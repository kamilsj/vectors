from __future__ import annotations

import json
import math
from collections.abc import Iterable, Iterator, Mapping
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
from typing import Any
from urllib.parse import quote, urlsplit

import httpx

from .errors import APIError, ProtocolError


def base_url(value: str) -> str:
    parsed = urlsplit(value)
    if (
        parsed.scheme not in ("http", "https")
        or not parsed.hostname
        or parsed.query
        or parsed.fragment
        or parsed.username
        or parsed.password
    ):
        raise ValueError(
            "base_url must be an http(s) server URL without credentials, query, or fragment"
        )
    return value.rstrip("/") + "/"


def segment(value: str) -> str:
    if not value or value in (".", ".."):
        raise ValueError("path identifiers must be nonempty and cannot be '.' or '..'")
    return quote(value, safe="")


def encode(value: Any) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, allow_nan=False, separators=(",", ":")
    ).encode("utf-8")


def retry_after(value: str | None) -> float | None:
    if value is None:
        return None
    try:
        seconds = float(value)
    except ValueError:
        try:
            seconds = (
                parsedate_to_datetime(value) - datetime.now(timezone.utc)
            ).total_seconds()
        except (TypeError, ValueError, OverflowError):
            return None
    return max(0.0, seconds) if math.isfinite(seconds) else None


def decode(response: httpx.Response) -> Any:
    try:
        data = response.json()
    except ValueError:
        data = None
    if not response.is_success:
        error = data.get("error", {}) if isinstance(data, dict) else {}
        if not isinstance(error, dict):
            error = {}
        raise APIError(
            response.status_code,
            str(error.get("code", "http_error")),
            str(error.get("message", "server returned an unsuccessful response")),
            retry_after(response.headers.get("Retry-After")),
        )
    if not isinstance(data, (dict, list)):
        raise ProtocolError("expected a JSON object or array from server")
    return data


def batches(
    rows: Iterable[Mapping[str, Any]],
    *,
    batch_size: int,
    max_batch_bytes: int,
    options: dict[str, Any],
) -> Iterator[tuple[bytes, int]]:
    """Serialize each row once; retain at most one batch plus one lookahead row."""
    if (
        isinstance(batch_size, bool)
        or not isinstance(batch_size, int)
        or batch_size < 1
    ):
        raise ValueError("batch_size must be a positive integer")
    if (
        isinstance(max_batch_bytes, bool)
        or not isinstance(max_batch_bytes, int)
        or max_batch_bytes < 1
    ):
        raise ValueError("max_batch_bytes must be a positive integer")
    suffix = b"]," + encode(options)[1:] if options else b"]}"
    prefix = b'{"rows":['
    overhead = len(prefix) + len(suffix)
    buffer: list[bytes] = []
    size = overhead
    for row in rows:
        if not isinstance(row, Mapping):
            raise ValueError(
                "each input row must be a mapping of column names to values"
            )
        if any(not isinstance(key, str) for key in row):
            raise ValueError("column names must be strings")
        encoded = encode(dict(row))
        if len(encoded) + overhead > max_batch_bytes:
            raise ValueError(
                "one row exceeds max_batch_bytes; increase the budget or reduce the row"
            )
        if buffer and size + len(encoded) + 1 > max_batch_bytes:
            yield prefix + b",".join(buffer) + suffix, len(buffer)
            buffer, size = [], overhead
        size += len(encoded) + bool(buffer)
        buffer.append(encoded)
        if len(buffer) == batch_size:
            yield prefix + b",".join(buffer) + suffix, len(buffer)
            buffer, size = [], overhead
    if buffer:
        yield prefix + b",".join(buffer) + suffix, len(buffer)
