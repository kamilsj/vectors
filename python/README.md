# Vectors Python SDK

Python 3.10+ clients for the Vectors SQL/vector database and its GraphRAG API.
Both synchronous and asynchronous clients expose the same operations. The
distribution is named **`vectors-sdk`** and the import is **`vectors_sdk`**.

## Install with uv

To use this checkout before a registry release is available, add the local SDK
to your application's uv project (Python 3.10+):

```sh
uv add /absolute/path/to/vectors/python
```

For an editable dependency while developing both projects, use
`uv add --editable /absolute/path/to/vectors/python`. To install with pip, run
`python -m pip install /absolute/path/to/vectors/python`.

After the first PyPI release, the registry install commands will be
`uv add vectors-sdk` and `python -m pip install vectors-sdk`.
See the [release guide](https://github.com/kamilsj/vectors/blob/main/python/PUBLISHING.md)
for publishing setup.

To develop the SDK itself, run these commands from the repository root with
uv 0.12.10 or later:

```sh
uv sync --project python --locked
cargo run --bin vectors-server -- --data-dir ./vectors-data
```

Then, in a second terminal at the repository root, run
`uv run --project python --locked python python/examples/quickstart.py`.
The development interpreter defaults to Python 3.12 through `.python-version`;
the package supports Python 3.10+.

Use the server built from this revision for SQL parameters and the corrected
cosine operator. Older servers can use `execute(sql)` without parameters and
the existing structured search/graph endpoints.

## SQL and vector search

```python
from vectors_sdk import Client, QueryResult

with Client("http://127.0.0.1:8080") as db:
    db.execute("""
        CREATE TABLE passages (
            id INTEGER PRIMARY KEY, text TEXT, tenant TEXT, embedding VECTOR(3)
        );
        CREATE INDEX passages_tenant ON passages USING HASH (tenant);
    """)
    db.insert("passages", [
        {"id": 1, "text": "WAL recovery", "tenant": "acme", "embedding": [1, 0, 0]},
        {"id": 2, "text": "Graph connections", "tenant": "acme", "embedding": [0, 1, 0]},
    ])
    result = db.execute("""
        SELECT id, text, embedding <=> $1 AS distance
        FROM passages WHERE tenant = $2 ORDER BY distance LIMIT $3
    """, [[1, 0, 0], "acme", 5])[0]
    assert isinstance(result, QueryResult)
    print(result.to_dicts())

    # The structured API bypasses SQL parsing and shares exact top-k execution.
    hits = db.search("passages", [1, 0, 0], select=["id", "text"], limit=5,
                     filters=[{"column": "tenant", "operator": "eq", "value": "acme"}])
    print(hits.rows_examined, hits.to_dicts())
```

`execute` returns a list of `QueryResult` and `CommandResult` objects, one per
statement. Query results carry columns, declared schema, rows, row count, and
rows examined. `to_dicts()` rejects duplicate column names; alias those columns
to avoid losing values. `explain(sql, parameters)` returns the server's
schema-aware query intent and optimization metadata without executing it.

Parameters use `$1`, `$2`, etc. Repeated references and out-of-order references
work. Supply `None`, booleans, signed 64-bit integers, finite floats, strings,
or nonempty lists of finite numbers (vectors). Every supplied parameter must
be used. Values cannot substitute table names, column names, or SQL fragments.
The server validates all bindings before execution, preserves multi-statement
write atomicity and response limits, and persists bound values in the WAL.
Parameter expansion is capped at 32 MiB and 65,535 inputs. This is value binding,
not a server-side prepared-statement or reusable-plan API.

## Bounded imports

```python
from vectors_sdk import BulkInsertError, Client

def passages():
    for number in range(100_000):
        yield {"id": number, "text": f"Passage {number}", "tenant": "acme",
               "embedding": [1.0, 0.0, 0.0]}

with Client(timeout=120) as db:
    try:
        for batch in db.iter_insert("passages", passages(), batch_size=1000,
                                    max_batch_bytes=4 * 1024 * 1024,
                                    on_conflict="do_nothing", conflict_target="id"):
            print(batch.input_offset, batch.row_count, batch.rows_affected)
    except BulkInsertError as error:
        print("Acknowledged input rows:", error.input_offset)
        print("Affected rows:", error.rows_affected)
        raise
```

Rows are serialized once and accumulated only until the next row or byte
boundary. The byte limit includes the exact UTF-8 JSON body, options, and
punctuation. One oversized row is rejected. At most one batch and one lookahead
row are buffered, with transient copies while assembling the HTTP body; the
SDK never loads the entire iterable. Empty iterables send no requests.
`insert(...)` consumes the iterator and returns an `IngestSummary`.

Each batch commits atomically; the complete import is **not** one transaction.
On failure, `BulkInsertError` includes `input_offset`, `rows_affected`,
`batches_completed`, and the underlying `cause`. An offset counts acknowledged
input rows, including conflict-skipped rows. A failed request may already have
committed if its response was lost. Reconcile that batch using stable IDs and
an explicit conflict policy before resuming. Invalid later input can also leave
earlier batches committed. Writes are never automatically retried.

Use `db.settings()["limits"]` to choose budgets for deployments with stricter
limits. Defaults are 1,000 rows and 4 MiB per SDK batch. `do_update` additionally
requires `conflict_target` and `update_columns`; `normalize_vectors=True`
normalizes non-null vectors on the server.

## GraphRAG

Configure embedding credentials and model in the server's Settings workspace.
Keys stay on the server; SDK `token` is the database bearer token. Graph
collections pin a provider/model/dimension profile.

```python
from vectors_sdk import Client

with Client(timeout=120) as db:
    db.create_collection("knowledge", semantic_neighbors=3, semantic_threshold=0.8,
                         document_columns=[{"name": "team", "data_type": "TEXT",
                                            "nullable": False}])
    graph = db.collection("knowledge")
    graph.ingest("storage-guide", "# Recovery\nCommitted writes are replayed from the WAL.",
                 title="Storage guide", source="handbook/storage.md",
                 metadata={"team": "platform"},
                 chunking={"max_characters": 1200, "overlap_characters": 150, "max_chunks": 256})
    result = graph.retrieve("How do committed writes recover?", candidate_limit=40,
                            max_results=10, max_hops=1, diversity=0.3,
                            max_per_document=3, max_context_bytes=24_000)
    for hit in result["hits"]:
        print(hit["document_id"], hit["start_byte"], hit["end_byte"], hit["text"])
    if result["hits"]:
        connections = graph.neighborhood(result["hits"][0]["chunk_id"], direction="both",
                                          kind="semantic", min_weight=0.8)
        print(connections["nodes"], connections["edges"])
```

`retrieve` exposes BM25/vector weighting, bounded graph expansion, local or
Voyage reranking, diversity, and context budgets. Use `direction="incoming"`
or `"both"`, an optional exact `kind`, and `min_weight` to control followed
relationships; the default remains outgoing links of any kind. These controls
filter graph expansion and returned relationships, while direct vector/BM25
matches remain eligible. For example:

```python
result = graph.retrieve("What supports this recovery procedure?",
                        direction="incoming", kind="supports", min_weight=0.5)
```

Use `max_seeds_per_document=1` to spread graph traversal across documents when
one long source dominates the highest-ranked chunks. The optional cap accepts
1..20; omission or `None` preserves the original seed ranking. It controls which
passages start graph exploration, while `max_per_document` limits returned
passages. For example:

```python
result = graph.retrieve("How do retries interact with recovery?",
                        seed_limit=8, max_seeds_per_document=1,
                        max_per_document=3)
```

Query embeddings are still generated when `vector_weight=0` for returned cosine
diagnostics; graph query fit then uses lexical evidence. `search` uses the
simpler vector-seed graph search. Connected hits may include `retrieval_path`
with a `seed_chunk_id` and up to three directed `edges`, ordered along the
traversal from seed to hit. Edge endpoints keep their stored direction even
when traversed backward. Bridges in that path need not appear in final hits.
Graph results remain dictionaries, preserving citation, score, revision, usage,
and truncation fields. Source offsets count **UTF-8 bytes**, not Python string
characters: `source.encode("utf-8")[start_byte:end_byte].decode("utf-8")`.

Other operations:

| Method | Behavior |
| --- | --- |
| `db.health()`, `db.readiness()` | Liveness and capacity/storage readiness |
| `db.tables()`, `db.schema(table)`, `db.indexes(table)` | SQL catalog inspection |
| `db.embedding_settings()`, `db.embed(texts, input_type="query")` | Inspect profile; generate embeddings |
| `db.preview_chunks(text, title=..., chunking=...)` | Preview source-aware chunks without a provider call |
| `db.collections()`, `graph.info()` | Inspect collections and current revision |
| `graph.document(id)` | Read the original source document |
| `graph.browse(offset=0, limit=100)` | Paged nodes and bounded edges; honor returned effective limit |
| `graph.upsert_relationship(a, b, "supports", weight=0.9, expected_revision=...)` | Add/update a directed relationship |
| `graph.delete_relationship(a, b, "supports", expected_revision=...)` | Remove a relationship |
| `graph.delete_document(id, expected_revision=...)` | Atomically remove a document, chunks, and incident edges |

The graph revision is **database-wide**. Serialize ingestion when loading a
collection to avoid conflicts during embedding generation. A stale revision
returns `APIError` with status 409 and code `stale_revision`. Refresh and
reconcile before retrying; provider work may already have incurred usage.

Declared document fields are canonical indexed SQL columns, merged into the
metadata returned by document and retrieval APIs. `TEXT`, `INTEGER`, `DOUBLE`,
and `BOOLEAN` fields support `nullable` and `unique`; missing nullable values
become null. Metadata-only ingestion updates reuse vectors and chunk links and
report `embeddings_reused: True` with zero provider token usage.

## Relationships between tables

Connect different kinds of data through matching scalar fields. For example,
after creating `articles(product_id INTEGER, ...)` and `products(id INTEGER, ...)`:

```python
links = db.relationships()
saved = db.create_relationship(
    "article_product",
    source_table="articles", source_column="product_id",
    target_table="products", target_column="id",
    expected_revision=links["revision"],
)
rows = db.execute("""
    SELECT a.product_id, p.id
    FROM articles a LEFT JOIN products p ON a.product_id = p.id
    LIMIT 100
""")[0]
print(rows.to_dicts())
db.delete_relationship("article_product", expected_revision=saved["revision"])
```

These methods also exist on `AsyncClient`. Creation atomically saves the
definition and missing lookup indexes; deletion keeps records and indexes.
Named links do not enforce foreign keys or automatically extend GraphRAG
traversal. SQL supports INNER/LEFT scalar equijoin chains across up to 16 tables,
including scalar filters, vector scores, ordering, and limits. Each ON must
connect an earlier table to the new table. Aggregate joins and join explain
plans remain unsupported.
See [structured data and relationships](../docs/STRUCTURED_DATA.md).

For a collection declaring `product_id` and `published` document fields, scope
hybrid retrieval without re-embedding or copying business facts into passages:

```python
result = db.collection("manuals").retrieve(
    "How do I maintain this product?",
    document_filters=[
        {"column": "product_id", "operator": "eq", "value": 7},
        {"column": "published", "operator": "eq", "value": True},
    ],
    max_hops=2,
)
```

Up to 32 AND-combined predicates use `eq`, `ne`, `gt`, `gte`, `lt`, or `lte`.
`eq`/`ne` with `None` mean IS NULL/IS NOT NULL. Filters use canonical document
columns before vector/keyword top-k and every graph hop, so excluded documents
cannot re-enter as graph context or bridges. Omit `document_filters` or pass
`[]` for the previous behavior. These are retrieval selectors, not per-user
permissions; BM25 corpus statistics remain collection-wide.

For filters and output fields belonging to business tables, query the full
path directly with a vector from the collection's embedding model:

```python
rows = db.execute("""
    SELECT c.text, d.title, p.name, c.embedding <=> $1 AS distance
    FROM graph_manuals_chunks c
    JOIN graph_manuals_documents d ON c.document_id = d.document_id
    JOIN products p ON d.product_id = p.id
    WHERE d.published = TRUE AND p.price <= $2
    ORDER BY distance LIMIT 10
""", params=[query_vector, 50.0])[0]
```

Graph and business tables share one query snapshot and durable storage in
Vectors. A separate PostgreSQL server is not queried or synchronized by the SDK.

## Async applications

```python
import asyncio
from vectors_sdk import AsyncClient

async def main():
    async with AsyncClient(max_connections=8) as db:
        # Bound application concurrency instead of creating millions of tasks.
        results = await asyncio.gather(
            db.search("passages", [1, 0, 0]),
            db.search("passages", [0, 1, 0]),
        )
        print([result.to_dicts() for result in results])

asyncio.run(main())
```

All network methods are awaitable. `collection(name)` creates a local handle
synchronously. Consume async `iter_insert` using `async for`; both clients
accept ordinary iterables/generators of rows. Avoid blocking data sources in
an async iterable. Cancellation propagates and does not retry a write; server
work may continue. Use `close()` / `await aclose()` if not using context managers.

## Connections, errors, and scale

One client owns a reusable HTTP connection pool, with 20 connections and a
30-second timeout by default. Configure `httpx.Timeout` for separate connect,
read, write, and pool timeouts. See the [HTTPX client documentation](https://www.python-httpx.org/advanced/clients/)
and [timeout controls](https://www.python-httpx.org/advanced/timeouts/).
Environment proxies are disabled unless `trust_env=True`; TLS verification is
enabled and redirects are never followed.

Provider-free read methods retry only explicit `503 overloaded` responses,
twice by default. They honor `Retry-After` up to 30 seconds; larger values are
returned to the application as an error without an early retry. Network errors,
SQL, writes, and embedding/reranking requests never retry automatically.
`APIError` exposes `status_code`, `code`, `message`, and `retry_after`;
`TransportError` describes uncertain network outcomes; `ProtocolError` reports
an invalid response. All inherit from `VectorsError`.

These controls reduce client memory and connection overhead. The engine still
uses exact vector scans and a memory-resident catalog. Graph collections retain
their existing 10,000-chunk and 64 MiB text limits; ANN indexing, partitioning,
replication, and distributed operation are future work. Validate your real
corpus and concurrency before treating this as a high-scale deployment.

## Development

```sh
uv sync --project python --locked
uv run --project python --locked python python/tools/generate_async.py --check
uv run --project python --locked ruff check python
cargo build --locked --bin vectors-server
VECTORS_TEST_SERVER=target/debug/vectors-server uv run --project python --locked python -m unittest discover -s python/tests -v
uv build python --out-dir python/dist --clear --no-sources
VECTORS_TEST_SERVER=target/debug/vectors-server uv run --project python --locked python python/tools/check_dist.py python/dist --install
```

The live tests start their own authenticated server in temporary storage, use
no external embedding provider, and shut it down after the suite. To change
client behavior, edit `_client.py` and run
`uv run --project python --locked python python/tools/generate_async.py`.
The checked-in async client has explicit signatures and is tested for parity.

On PowerShell, set `$env:VECTORS_TEST_SERVER = "target/debug/vectors-server.exe"`
before running the same uv commands, without their environment-variable prefix.
Without `VECTORS_TEST_SERVER`, the live tests are skipped. CI tests Python 3.10
and 3.14 on Linux and Windows; the release job also tests clean wheel and source
distribution installs against a live server.

`uv.lock` pins runtime and development dependencies for contributors. Refresh
it with `uv lock --project python --upgrade` when intentionally updating them,
then rerun these checks. Applications resolve the SDK's declared HTTPX range
in their own environment; they do not inherit SDK development dependencies.
