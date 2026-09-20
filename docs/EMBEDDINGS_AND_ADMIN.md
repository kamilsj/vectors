# Text embeddings and data administration

The console has five workspaces: **Search**, **Connections**, **Data**, **SQL**,
and **Settings**. Search accepts ordinary text or an existing vector. Connections
chunks documents, displays their relationships, and retrieves context with
optional reranking; see the [GraphRAG guide](GRAPH_RAG.md). Data manages tables
and rows. SQL keeps the full query editor available. Help contains the tutorial.

**Data → Create** offers RAG collections and structured tables through a shared
field editor. Collections accept optional typed document fields; the document
form then shows those fields alongside title, source, and text. **View data**
opens the underlying documents table, and **Open SQL** prepares a chunks/documents
join. Metadata-only document edits reuse existing embeddings.

The Data **Relationships** panel links matching scalar fields across tables,
including collection document fields and ordinary business records. It manages
named definitions with revision checks and opens their SQL joins. Removing a
definition preserves records and indexes. See [structured data and
relationships](STRUCTURED_DATA.md) for the HTTP endpoints and current SQL scope.

## Connect an embedding provider

1. Open **Settings** and choose OpenAI or Voyage AI.
2. Choose a model and output dimensions. Dimensions must match the target
   `VECTOR(n)` column.
3. Enter the provider key and save, or start the server with `OPENAI_API_KEY`
   or `VOYAGE_API_KEY` set in its environment.
4. Choose batch size, request timeout, and concurrent request capacity as needed.

Provider keys entered in the console are sent to your vectors server and kept
only in its memory. They are never returned by the settings API or saved in
browser storage. Restarting the server discards entered keys and reloads its
environment. Clearing a key disables it for the current process; remove the
corresponding environment variable to keep it disabled after restarting.

When using `--data-dir`, non-secret provider settings are saved in
`embedding-settings.json` in that directory. Without durable storage, settings
last until restart. Existing rows and vectors are never re-embedded by changing
settings.

The initial model is OpenAI `text-embedding-3-small`, with 1,536 dimensions.
`text-embedding-3-large` defaults to 3,072 dimensions. Both accept smaller
positive output dimensions. Voyage `voyage-4`, `voyage-4-large`, and
`voyage-4-lite` support 256, 512, 1,024, and 2,048 dimensions, with 1,024 as the
default. These options follow the
[OpenAI embedding guide](https://developers.openai.com/api/docs/guides/embeddings)
and [Voyage embedding guide](https://docs.voyageai.com/docs/embeddings).

Use the same provider, model, and dimensions for a collection's documents and
search queries. Equal dimensions alone do not make different models compatible.
For existing vectors, select the configuration used to generate them; the
database cannot infer their model from the numbers. When migrating models,
create a separate vector column or table and regenerate its vectors.

Text is sent to the selected provider only when you explicitly generate
embeddings or run a text search. Provider usage is billed to the configured
provider account. Tests use synthetic local provider responses and do not
require paid API calls.

## Search and add documents

In **Search**, select a table and vector column, enter a question, and run the
search. The console generates a query embedding, then uses the existing exact
vector-search API. Advanced options accept raw vectors, change the distance
metric, add a relational filter, and select returned columns.

In **Data**, create or select a table. Use a unique scalar identifier to make
individual rows editable. A typical document schema contains an integer `id`,
a text `title`, a text `content`, and an `embedding VECTOR(n)` column.

Add rows directly as JSON, or select the text source and vector destination
columns and generate embeddings before inserting them. Review the rows and
selected model before submitting. Existing non-null embeddings are not silently
replaced. Generation and insertion are separate operations: provider success
does not mean the database write succeeded. If insertion fails, inspect the
error and existing data before retrying.

Voyage generation uses `input_type: "document"` for stored text and
`input_type: "query"` for search. OpenAI receives the corresponding text without
an input-type field. Both providers return floating-point vectors.

## Manage current data

The Data view fetches only the selected page. Select a row to edit or delete
it. Row mutations require a non-null, scalar column with a unique constraint;
tables without one remain browsable and support insertion through the typed
API. Row updates and deletes carry the revision from the inspected page. If
the data changed meanwhile, refresh before retrying. The revision comparison
and write commit happen under one database lock, so a concurrent writer cannot
slip between them.

Deleting a table requires typing its name. SQL remains available for complex
bulk operations. All data mutations retain existing transaction, validation,
and durable WAL behavior. Administrative routes use the same bearer-token
policy as the other `/v1` endpoints; there is no separate role system.

Settings also provides browser preferences for result page size, automatic
refresh, and compact rows, plus the server's effective storage, compute,
capacity, and request limits. Browser preferences are local to that browser;
server capacity and compute settings are startup settings and require a restart
to change.

## Embedding API

All examples assume a local server and an optional `VECTORS_API_TOKEN`.
Provider credentials are separate from that token.

```sh
curl http://127.0.0.1:8080/v1/settings/embeddings \
  -H "Authorization: Bearer $VECTORS_API_TOKEN"

curl -X PUT http://127.0.0.1:8080/v1/settings/embeddings \
  -H "Authorization: Bearer $VECTORS_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"provider":"openai","model":"text-embedding-3-small","dimensions":1536,"batch_size":32}'

curl http://127.0.0.1:8080/v1/embeddings \
  -H "Authorization: Bearer $VECTORS_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"input":["A document about vector databases"],"input_type":"document"}'
```

Generation returns `provider`, `model`, `dimensions`, `input_type`, ordered `embeddings`, and
`usage.total_tokens`. Pass a returned vector as `query` to `/v1/vector/search`,
or include it in a row sent to `/v1/tables/{table}/rows`. The existing
`/v1/embeddings/search` route remains an alias for raw-vector search.

An optional `expected_settings` object containing `provider`, `model`, and
`dimensions` rejects a request if the saved configuration changed before it
started. Settings responses never contain keys. PUT accepts an optional
write-only `api_key` for the selected provider, or `clear_api_key: true`.

Generation allows up to 256 non-empty texts and 1 MiB of combined UTF-8 text.
Each input is conservatively capped at 8,191 UTF-8 bytes for OpenAI or 31,872
for Voyage (which reserves space for its retrieval prompt). These are byte
budgets, not exact tokenizer counts. Batches also respect conservative totals
for the selected provider/model. Voyage receives explicit `document`/`query`
roles and `truncation: false`; OpenAI has no separate role parameter.
The limits follow the [OpenAI embedding API](https://developers.openai.com/api/reference/resources/embeddings/methods/create)
and [Voyage embedding API](https://docs.voyageai.com/docs/embeddings).
Configured batch size is
1–128, timeout is 1–120 seconds, and concurrent request capacity is 1–16. These
bounds apply independently of database query capacity. Batches share a pooled
HTTP client. Requests are never retried automatically; a failed response may
still represent provider usage. Responses are checked for count, index,
dimension, and finite numeric values before they are returned.

For complete documents, use the [chunking and GraphRAG workflow](GRAPH_RAG.md)
to preserve source citations, pin the embedding profile, generate chunk vectors,
and build relationships in one atomic document ingestion operation.

## Administrative API

| Method and path | Purpose |
| --- | --- |
| `GET /v1/settings/server` | Read effective server settings without secrets |
| `GET /v1/admin/tables/{table}/rows?limit=50&offset=0` | Fetch a coherent page with schema, total rows, and revision |
| `POST /v1/admin/tables` | Create a typed table |
| `POST /v1/tables/{table}/rows` | Insert typed rows using the existing API |
| `PATCH /v1/admin/tables/{table}/rows` | Update one uniquely identified row |
| `DELETE /v1/admin/tables/{table}/rows` | Delete one uniquely identified row |
| `DELETE /v1/admin/tables/{table}` | Delete a table after checking its name and revision |

Page sizes are 1–250 and cannot exceed the server's configured response limit.
A page returns `table`, `columns`, `schema`, `rows`, `total_rows`, `limit`,
`offset`, and `revision`. Use the returned revision in mutations:

```json
{
  "expected_revision": 3,
  "key": {"column": "id", "value": 1},
  "values": {"title": "Revised title"}
}
```

DELETE-row uses the same body without `values`. DELETE-table takes
`{"expected_revision":3,"confirm_table":"documents"}`. A stale revision
returns HTTP 409 with `error.code: "stale_revision"`. Durable databases use the
persisted WAL commit sequence, so revisions and conflict protection survive
normal restarts and updates. In-memory and snapshot-only sessions have
process-local revisions. Revisions are opaque tokens, not row counts: a durable
multi-statement transaction advances once. Refresh existing browser pages after
upgrading from builds with process-local durable revisions, or restoring an
older database backup.

Create-table accepts a name and a list of typed columns:

```json
{
  "name": "notes",
  "columns": [
    {"name": "id", "data_type": "INTEGER", "nullable": false, "unique": true},
    {"name": "content", "data_type": "TEXT", "nullable": false, "unique": false},
    {"name": "embedding", "data_type": "VECTOR(1536)", "nullable": true, "unique": false}
  ]
}
```

Types are `INTEGER`, `DOUBLE`, `TEXT`, `BOOLEAN`, and `VECTOR(n)`. Names are
quoted internally; values are validated against the actual schema. Mutations
return the same `results` envelope as the SQL API. Use **SQL** when a workflow
needs functionality beyond these individual-row administration tools.
