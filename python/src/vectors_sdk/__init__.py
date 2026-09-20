"""Python SDK for Vectors: SQL, exact vector search, and GraphRAG."""

from ._async_client import AsyncClient, AsyncCollection
from ._client import Client, Collection
from ._version import __version__
from .errors import (
    APIError,
    BulkInsertError,
    ProtocolError,
    TransportError,
    VectorsError,
)
from .models import Column, CommandResult, IngestBatch, IngestSummary, QueryResult

__all__ = [
    "__version__",
    "Client",
    "AsyncClient",
    "Collection",
    "AsyncCollection",
    "APIError",
    "BulkInsertError",
    "ProtocolError",
    "TransportError",
    "VectorsError",
    "Column",
    "CommandResult",
    "IngestBatch",
    "IngestSummary",
    "QueryResult",
]
