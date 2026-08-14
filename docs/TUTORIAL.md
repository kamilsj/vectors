# Learn vectors

`vectors` keeps embeddings and ordinary application data in one table, then
lets SQL filter and rank both in one statement. This tutorial starts with a
local sample and ends with the durable HTTP server, typed ingestion, GPU
selection, and practical operating advice.

> **What search does today**
>
> Vector ranking is exact: every candidate left after relational filtering is
> scored. It is deterministic and useful as a correctness reference, but it is
> not an approximate-nearest-neighbor index. Use selective metadata filters for
> large collections and benchmark with your own dimensions and candidate counts.

## Choose a path

| I want to... | Start here |
| --- | --- |
| learn the SQL interactively | [Use the shell](#1-use-the-shell) |
| explore visually | [Use the web console](#5-use-the-web-console) |
| send requests from an application | [Use the HTTP API](#6-use-the-http-api) |
| keep data across restarts | [Run with durable storage](#4-run-with-durable-storage) |
| tune large exact scans | [Configure CPU and GPU execution](#9-configure-cpu-and-gpu-execution) |

## Install

The release installers select the native archive, verify its SHA-256 checksum,
install both `vectors` and `vectors-server` for the current user, start a
durable local server, and open the web console when a desktop is available.
Native releases cover Linux x86-64 and ARM64, macOS Intel and Apple silicon,
and Windows x86-64.

### Linux or macOS

```sh
installer="$(mktemp)"
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/kamilsj/vectors/releases/latest/download/install.sh \
  -o "$installer"
sh "$installer"
rm -f "$installer"
```

The binaries go to `~/.local/bin`. Linux follows XDG data/state locations;
macOS uses `~/Library/Application Support/vectors` for data and state and
`~/Library/Logs/vectors` for logs.

To install without starting the server, replace the `sh` command above with:

```sh
sh "$installer" --no-start --no-open
```

### Windows x86-64

Run this in PowerShell:

```powershell
$Installer = Join-Path $env:TEMP 'vectors-install.ps1'
Invoke-WebRequest `
  -Uri 'https://github.com/kamilsj/vectors/releases/latest/download/install.ps1' `
  -OutFile $Installer
Unblock-File $Installer
& $Installer
Remove-Item $Installer
```

The default binaries go to `%LOCALAPPDATA%\Programs\vectors`. Durable data and
server state live below `%LOCALAPPDATA%\vectors`.

To install without starting the server, pass these switches before removing
the downloaded file:

```powershell
& $Installer -NoStart -NoOpen
```

Both installers accept a release version, custom install directory, bind
address, install-only mode, and no-browser mode:

| Installer setting | Linux/macOS | Windows |
| --- | --- | --- |
| inspect target without downloading | `--print-target` | `-PrintTarget` |
| preview the complete plan | `--dry-run` | `-WhatIf` |
| fixed version | `--version v0.6.0` or `VECTORS_VERSION` | `-Version v0.6.0` or `VECTORS_VERSION` |
| install directory | `--install-dir PATH` or `VECTORS_INSTALL_DIR` | `-InstallDir PATH` or `VECTORS_INSTALL_DIR` |
| listen address | `--bind 127.0.0.1:8081` or `VECTORS_BIND` | `-BindAddress 127.0.0.1:8081` or `VECTORS_BIND` |
| do not start | `--no-start` or `VECTORS_NO_START=1` | `-NoStart` or `VECTORS_NO_START=1` |
| do not open a browser | `--no-open` or `VECTORS_NO_OPEN=1` | `-NoOpen` or `VECTORS_NO_OPEN=1` |
| restart an installed server after upgrade | `--restart` | automatic for an installer-managed server |
| process-state directory | `VECTORS_STATE_DIR` | `VECTORS_STATE_DIR` |

Save an installer locally and run `sh ./install.sh --help` or
`Get-Help .\install.ps1 -Full` to inspect every option before running it.
The complete [installation guide](INSTALL.md) covers upgrades, uninstalling,
release verification, default paths, and troubleshooting.

### Build from source

The crate requires Rust 1.89 or newer:

```sh
git clone https://github.com/kamilsj/vectors.git
cd vectors
cargo build --release
```

Add the optional GPU backend when you need it:

```sh
cargo build --release --features gpu
```

The default Cargo feature set stays CPU-only. Official release binaries are
built with GPU support, while their default `auto` policy still falls back to
the CPU on machines without a compatible adapter.

## 1. Use the shell

Start a temporary, in-memory database:

```sh
vectors
```

When running from a source checkout, the equivalent command is:

```sh
cargo run --release --bin vectors
```

The shell executable accepts these options:

| Option | Meaning |
| --- | --- |
| `--data-dir PATH` | Open or create a durable WAL-backed database. |
| `-h` / `--help` | Show shell usage. |
| `-V` / `--version` | Print the installed version. |

The opening prompt points to the built-in lesson:

```text
vectors 0.6.0 | in-memory SQL vector database
Type .tutorial to begin, .help for commands. End SQL with ';'.
vectors>
```

Type `.tutorial` to print a copy-ready walkthrough. From a repository checkout,
you can execute the repeatable version directly:

```text
vectors> .read examples/quickstart.sql
```

SQL may span several lines and runs when the final statement ends with `;`.
Shell commands begin with `.` and do not use a semicolon.

### Every shell command

| Command | What it does |
| --- | --- |
| `.help` | Show the complete command reference. |
| `.tutorial` | Print the built-in vector-search walkthrough. |
| `.tables` | List tables in the current database. |
| `.schema TABLE` | Show column names, SQL types, nullability, and uniqueness. |
| `.indexes TABLE` | Show scalar hash indexes for a table. |
| `.checkpoint` | Compact a durable database's WAL into `vectors.vdb`. |
| `.save PATH` | Write a portable snapshot of the current database. |
| `.open PATH` | Open a snapshot as an in-memory session. |
| `.read PATH` | Execute all SQL statements in a text file. |
| `.timer on` / `.timer off` | Enable or disable execution-time output. |
| `.cancel` | Discard the SQL currently waiting at the multiline prompt. |
| `.quit` / `.exit` | Exit the shell. |

Quote a path that contains spaces:

```text
vectors> .read "my queries/import.sql"
vectors> .save "documents backup.vdb"
```

If the prompt changes to `...>`, the shell is waiting for a closing quote,
comment, or semicolon. Finish the statement or enter `.cancel`.

## 2. Model relational data and embeddings together

Create a table with a fixed-width embedding and a scalar index for the metadata
you plan to filter:

```sql
CREATE TABLE IF NOT EXISTS documents (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    category TEXT NOT NULL,
    published BOOLEAN NOT NULL,
    embedding VECTOR(3) NOT NULL
);

CREATE INDEX IF NOT EXISTS documents_category_idx
ON documents USING HASH (category);
```

`VECTOR(3)` means every non-null value must contain exactly three finite `f32`
components. Real embedding models commonly use larger dimensions; put the
model's actual width in the schema.

Insert relational values and vectors in the same row. `ARRAY[...]` and
`VECTOR(...)` are equivalent vector literal forms:

```sql
INSERT INTO documents VALUES
    (1, 'Rust for data systems', 'tech', TRUE,  ARRAY[1.0, 0.0, 0.0]),
    (2, 'A practical cooking guide', 'food', TRUE, ARRAY[0.0, 1.0, 0.0]),
    (3, 'Inside database engines', 'tech', TRUE, ARRAY[0.82, 0.18, 0.0]),
    (4, 'A draft on storage', 'tech', FALSE, VECTOR(0.72, 0.20, 0.08))
ON CONFLICT (id) DO UPDATE SET
    title = excluded.title,
    category = excluded.category,
    published = excluded.published,
    embedding = excluded.embedding;
```

For an import that may be replayed, make the conflict behavior explicit:

```sql
INSERT INTO documents VALUES
    (3, 'Database internals, revised', 'tech', TRUE, ARRAY[0.90, 0.10, 0.0])
ON CONFLICT (id) DO UPDATE SET
    title = excluded.title,
    category = excluded.category,
    published = excluded.published,
    embedding = excluded.embedding;
```

`ON CONFLICT DO NOTHING` is also supported. A multi-statement request is atomic:
if any statement fails validation or persistence, none of its mutations become
visible.

## 3. Run exact hybrid search

A hybrid query first uses ordinary SQL predicates to find eligible rows, then
ranks those candidates by vector distance:

```sql
SELECT id,
       title,
       cosine_distance(embedding, ARRAY[1.0, 0.0, 0.0]) AS distance
FROM documents
WHERE category = 'tech'
  AND published = TRUE
ORDER BY distance ASC
LIMIT 5;
```

Distance functions sort nearest-first with `ASC`. Similarity functions such as
`dot_product` sort best-first with `DESC`:

```sql
SELECT id,
       title,
       dot_product(embedding, ARRAY[1.0, 0.0, 0.0]) AS similarity
FROM documents
WHERE category = 'tech'
ORDER BY similarity DESC
LIMIT 5;
```

### Vector functions and operators

| SQL | Meaning | Nearest-neighbor order |
| --- | --- | --- |
| `cosine_distance(a, b)` | cosine distance | `ASC` |
| `l2_distance(a, b)` / `euclidean_distance(a, b)` | Euclidean distance | `ASC` |
| `squared_l2_distance(a, b)` | squared Euclidean distance | `ASC` |
| `dot_product(a, b)` / `inner_product(a, b)` | dot-product similarity | `DESC` |
| `a <=> b` | cosine distance | `ASC` |
| `a <-> b` | Euclidean distance | `ASC` |
| `a <#> b` | negative dot product | `ASC` |
| `vector_dims(v)` / `dimensions(v)` | vector width | not a ranking function |
| `vector_norm(v)` / `norm(v)` | L2 norm | not a ranking function |
| `normalize(v)` / `normalize_vector(v)` | unit-length vector | not a ranking function |

The operator form makes compact SQL possible:

```sql
SELECT id, title, embedding <=> ARRAY[1.0, 0.0, 0.0] AS distance
FROM documents
WHERE category = 'tech'
ORDER BY distance
LIMIT 5;
```

Cosine distance and normalization require non-zero vectors. Dimensions must
match; the engine rejects mismatches instead of silently truncating values.

The relational side supports comparisons, boolean logic, arithmetic, `NULL`,
`BETWEEN`, `IN`, `LIKE`, `ILIKE`, aliases, `DISTINCT`, grouping, aggregates,
`HAVING`, ordering, limits, and offsets. It also supports `UPDATE`, `DELETE`,
table/index creation and removal, and SQL upserts. Joins, subqueries, window
functions, and approximate vector indexes are not implemented yet.

### SQL at a glance

| Task | Statements and clauses |
| --- | --- |
| define data | `CREATE TABLE`, `DROP TABLE`, `PRIMARY KEY`, `UNIQUE`, `VECTOR(n)` |
| index metadata | `CREATE INDEX ... USING HASH`, `DROP INDEX` |
| change rows | multi-row `INSERT`, `UPDATE`, `DELETE` |
| retry writes safely | `ON CONFLICT DO NOTHING`, `ON CONFLICT (...) DO UPDATE` |
| read and filter | `SELECT`, `WHERE`, aliases, `DISTINCT` |
| summarize | `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `GROUP BY`, `HAVING` |
| rank and page | `ORDER BY`, `LIMIT`, `OFFSET` |
| inspect a plan | `EXPLAIN SELECT ...` |

## 4. Run with durable storage

The in-memory shell is convenient for experiments. Add a data directory when
the data must survive a restart:

```sh
vectors --data-dir ./vectors-data
```

Or start the HTTP server in durable mode:

```sh
vectors-server --data-dir ./vectors-data
```

The directory contains:

- `vectors.wal`, the synchronized write-ahead log;
- `vectors.vdb`, the compact binary checkpoint; and
- `vectors.lock`, which prevents two processes from owning the directory.

Durability does not make searches disk-backed. The active catalog, dense vector
columns, and scalar indexes are loaded into host memory; size the machine for
the working set plus query and GPU-cache headroom.

Acknowledged durable writes are synchronized to the WAL before they become
visible. Startup loads the checkpoint and replays newer WAL records. The server
checkpoints on graceful shutdown and when the WAL reaches its compaction
threshold; from the durable shell you can request one with `.checkpoint`.

Portable snapshots are a separate workflow:

```text
vectors> .save documents.vdb
vectors> .open documents.vdb
```

`.open` replaces the database attached to that shell session with an in-memory
snapshot handle. Save anything you need before opening it, and remember that
subsequent writes are not protected by a WAL. Restart the shell with
`--data-dir PATH` when new writes must be durable. Prefer `--data-dir` for normal
operation and use `.save` for explicit exports or backups.

## 5. Use the web console

Start the server, then open [http://127.0.0.1:8080](http://127.0.0.1:8080):

```sh
vectors-server --data-dir ./vectors-data
```

If 8080 is busy, choose another local port:

```sh
vectors-server --data-dir ./vectors-data --port 8081
```

The standalone server accepts these command-line options:

| Option | Meaning |
| --- | --- |
| `-p PORT` / `--port PORT` | Listen on `127.0.0.1:PORT`. |
| `--bind ADDRESS` | Listen on an explicit host and port such as `0.0.0.0:9000`. |
| `--data-dir PATH` | Open or create a durable WAL-backed database. |
| `--compute auto\|cpu\|gpu` | Choose exact vector-scan placement. |
| `-h` / `--help` | Show server usage. |
| `-V` / `--version` | Print the installed version. |

Where both forms exist, command-line settings take precedence:
`--port`/`--bind` override `VECTORS_BIND`, `--data-dir` overrides
`VECTORS_DATA_DIR`, and `--compute` overrides `VECTORS_COMPUTE_DEVICE`.

The console has three useful views:

1. **Start here** explains the workflow and loads the sample dataset.
2. **SQL console** runs multiline SQL, formats it, analyzes query intent, and
   shows typed result tables plus rows examined.
3. **Vector search** builds an exact hybrid query from table, vector, metric,
   scalar-filter, selected-column, and limit controls.

After running the quickstart, select `documents` in the sidebar to inspect its
schema and indexes. Choose **Understand query** before **Run query** when you
want a plain description of the selected columns, filters, ranking, and whether
the optimized top-k path is available.

When `VECTORS_API_TOKEN` is enabled, choose **API token** and paste the token.
The console keeps it in browser session storage, not permanent local storage.

## 6. Use the HTTP API

The server exposes the console and health endpoints publicly. All `/v1` routes
require `Authorization: Bearer ...` when `VECTORS_API_TOKEN` is configured.

| Method | Endpoint | Purpose |
| --- | --- | --- |
| `GET` | `/` | Built-in web console |
| `GET` | `/healthz` | Liveness, version, storage mode, and JSON body limit |
| `GET` | `/readyz` | Readiness, catalog revision, capacity, and request limits |
| `GET` | `/metrics` | Prometheus-compatible operational metrics |
| `POST` | `/v1/sql` | Execute one or more SQL statements |
| `POST` | `/v1/sql/intent` | Validate and describe one read-only `SELECT` |
| `GET` | `/v1/tables` | List tables and row counts |
| `GET` | `/v1/tables/{table}/schema` | Read typed column metadata |
| `GET` | `/v1/tables/{table}/indexes` | Read scalar index metadata |
| `POST` | `/v1/tables/{table}/rows` | Typed bulk insert or upsert |
| `POST` | `/v1/vector/search` | Structured exact hybrid search |
| `POST` | `/v1/embeddings/search` | Alias of `/v1/vector/search` |

### Execute SQL

Linux/macOS shell:

```sh
curl http://127.0.0.1:8080/v1/sql \
  -H 'content-type: application/json' \
  -d '{"sql":"SELECT id, title FROM documents ORDER BY id LIMIT 20"}'
```

A query result includes names, declared SQL types, rows, row count, and rows
examined. Types remain available even when no rows match:

```json
{
  "results": [
    {
      "type": "query",
      "columns": ["id", "title"],
      "schema": [
        {"name": "id", "data_type": "INTEGER"},
        {"name": "title", "data_type": "TEXT"}
      ],
      "rows": [[1, "Rust for data systems"]],
      "row_count": 1,
      "rows_examined": 4
    }
  ]
}
```

PowerShell:

```powershell
$body = @{
  sql = 'SELECT id, title FROM documents ORDER BY id LIMIT 20'
} | ConvertTo-Json

Invoke-RestMethod `
  -Uri 'http://127.0.0.1:8080/v1/sql' `
  -Method Post `
  -ContentType 'application/json' `
  -Body $body
```

### Understand query intent without executing it

The intent endpoint accepts exactly one `SELECT`. It expands `*`, validates the
query against the current catalog, labels outputs as identifiers, content,
attributes, embeddings, similarity scores, or aggregates, and describes vector
ranking. It never runs a mutation or guesses a missing table.

Linux/macOS shell:

```sh
curl http://127.0.0.1:8080/v1/sql/intent \
  -H 'content-type: application/json' \
  --data-binary @- <<'JSON'
{
  "sql": "SELECT id, embedding <=> ARRAY[1,0,0] AS d FROM documents ORDER BY d LIMIT 5"
}
JSON
```

### Ingest typed rows

Typed ingestion avoids generating and parsing large SQL literal lists. JSON
objects are matched to the table schema and use the same atomic constraint,
conflict, revision, WAL, and index-maintenance path as SQL insertion:

Linux/macOS shell:

```sh
curl http://127.0.0.1:8080/v1/tables/documents/rows \
  -H 'content-type: application/json' \
  -d '{
    "rows": [
      {
        "id": 5,
        "title": "Storage pages",
        "category": "tech",
        "published": true,
        "embedding": [0.76, 0.20, 0.04]
      },
      {
        "id": 6,
        "title": "A second recipe",
        "category": "food",
        "published": true,
        "embedding": [0.05, 0.90, 0.05]
      }
    ],
    "normalize_vectors": true,
    "on_conflict": "do_update",
    "conflict_target": "id",
    "update_columns": ["title", "category", "published", "embedding"]
  }'
```

`on_conflict` accepts `fail` (the default), `do_nothing`, or `do_update`.
`do_update` requires a unique `conflict_target` and at least one
`update_columns` entry. `normalize_vectors: true` normalizes every non-null
vector before insertion and rejects zero vectors.

The same request from PowerShell can use native objects, avoiding shell quoting:

```powershell
$body = @{
  rows = @(
    @{
      id = 7
      title = 'Query planning'
      category = 'tech'
      published = $true
      embedding = @(0.88, 0.10, 0.02)
    }
  )
  normalize_vectors = $true
  on_conflict = 'do_nothing'
  conflict_target = 'id'
} | ConvertTo-Json -Depth 5

Invoke-RestMethod `
  -Uri 'http://127.0.0.1:8080/v1/tables/documents/rows' `
  -Method Post `
  -ContentType 'application/json' `
  -Body $body
```

### Run structured vector search

Use this endpoint when a client should not construct SQL. Filters are combined
with `AND`; supported operators are `eq`, `ne`, `gt`, `gte`, `lt`, and `lte`.
The metric is `cosine`, `l2`, `squared_l2`, or `dot_product`.

Linux/macOS shell:

```sh
curl http://127.0.0.1:8080/v1/vector/search \
  -H 'content-type: application/json' \
  -d '{
    "table": "documents",
    "vector_column": "embedding",
    "query": [1.0, 0.0, 0.0],
    "metric": "cosine",
    "select": ["id", "title", "category"],
    "filters": [
      {"column": "category", "operator": "eq", "value": "tech"},
      {"column": "published", "operator": "eq", "value": true}
    ],
    "limit": 5
  }'
```

If `select` is empty, the response includes every scalar column and omits the
stored vector. The computed score is always returned as `distance`. A structured
search limit must be between 1 and 1,000.

### Authenticate API calls

Start the server with a long random token:

Linux/macOS shell:

```sh
VECTORS_API_TOKEN='replace-with-a-long-random-token' \
vectors-server --data-dir ./vectors-data
```

PowerShell:

```powershell
$env:VECTORS_API_TOKEN = 'replace-with-a-long-random-token'
vectors-server --data-dir .\vectors-data
```

Then add the header to each `/v1` request:

Linux/macOS shell:

```sh
curl http://127.0.0.1:8080/v1/tables \
  -H 'authorization: Bearer replace-with-a-long-random-token'
```

PowerShell:

```powershell
$headers = @{ Authorization = 'Bearer replace-with-a-long-random-token' }
$body = @{ sql = 'SELECT id, title FROM documents LIMIT 20' } | ConvertTo-Json
Invoke-RestMethod `
  -Uri 'http://127.0.0.1:8080/v1/sql' `
  -Method Post `
  -Headers $headers `
  -ContentType 'application/json' `
  -Body $body
```

There is no built-in TLS or per-user authorization. Keep the default localhost
bind for local use. If the service must be reachable across a network, put it
behind a TLS-enabled reverse proxy and protect the token as a secret.

## 7. Inspect plans with `EXPLAIN`

`EXPLAIN` shows how a `SELECT` will run without scanning its rows:

```sql
EXPLAIN
SELECT id,
       title,
       cosine_distance(embedding, ARRAY[1.0, 0.0, 0.0]) AS distance
FROM documents
WHERE category = 'tech'
ORDER BY distance
LIMIT 5;
```

A fast exact-search plan can report stages like these:

```text
Scan: scalar hash index on documents (...)
Filter: category = 'tech' (covered by scalar hash index)
Projection: id, title, distance
VectorTopK: distance ASC (...)
Compute: CPU exact scan (... candidates x 3 dimensions)
Limit: 5
```

Look for:

- `scalar hash index` to confirm metadata pruning;
- `VectorTopK` to confirm bounded top-k scoring and deferred projection;
- `Compute` to see CPU, GPU eligibility, or fallback information; and
- the candidate count to understand the work remaining after filters.

`EXPLAIN ANALYZE` is not supported. The plan is descriptive, not a timed query.
Use `.timer on` or an application-side measurement for latency.

## 8. Embed vectors in Rust

The library, shell, and server all use the same `Database` engine:

```rust
use vectors::{Database, ExecutionResult};

fn main() -> vectors::Result<()> {
    let database = Database::open_persistent("./vectors-data")?;
    database.execute(
        "CREATE TABLE IF NOT EXISTS points (
             id INTEGER PRIMARY KEY,
             label TEXT,
             embedding VECTOR(2)
         )",
    )?;
    database.execute(
        "INSERT INTO points VALUES (1, 'origin-ish', ARRAY[0.1, 0.2])
         ON CONFLICT (id) DO NOTHING",
    )?;

    let results = database.execute(
        "SELECT id, label,
                embedding <-> ARRAY[0.0, 0.0] AS distance
         FROM points
         ORDER BY distance
         LIMIT 10",
    )?;

    if let Some(ExecutionResult::Query(result)) = results.last() {
        println!("{} row(s), {} examined", result.row_count(), result.rows_examined);
    }
    database.checkpoint()?;
    Ok(())
}
```

Applications that already hold typed data can call `Database::insert_rows`
with `Value` and `Vector` values instead of turning them into SQL strings. See
[`examples/hybrid_search.rs`](../examples/hybrid_search.rs) for a complete
program.

## 9. Configure CPU and GPU execution

Compute policy changes how eligible `VectorTopK` scans are scored, not their SQL
meaning:

| Policy | Behavior |
| --- | --- |
| `cpu` | Always use the portable Rayon CPU path. |
| `auto` | Try the GPU above the configured crossover; safely fall back to CPU. |
| `gpu` | Require GPU execution for an eligible scan and return an error if it cannot run. |

Choose the server policy directly:

```sh
vectors-server --data-dir ./vectors-data --compute auto
```

Or configure the server and shell through environment variables:

```sh
VECTORS_COMPUTE_DEVICE=auto \
VECTORS_GPU_MIN_ELEMENTS=8388608 \
VECTORS_GPU_CACHE_BYTES=536870912 \
vectors-server --data-dir ./vectors-data
```

```powershell
$env:VECTORS_COMPUTE_DEVICE = 'auto'
$env:VECTORS_GPU_MIN_ELEMENTS = '8388608'
$env:VECTORS_GPU_CACHE_BYTES = '536870912'
vectors-server --data-dir .\vectors-data
```

The crossover is `candidate rows × vector dimensions`; it defaults to
8,388,608 elements. The dense-column GPU cache defaults to 512 MiB and remains
bounded. Device uploads may be sharded and dispatch/readback work is batched.

GPU execution is exhaustive, not ANN. It is considered only for a compatible
top-k plan without a residual row-by-row predicate. A predicate fully covered
by a scalar index is compatible; a more complex residual filter remains on the
CPU path. In `auto`, adapter, device, cache, or execution failures fall back to
the CPU. Use `EXPLAIN` and `--compute gpu` during controlled testing when you
must prove that an eligible workload reaches the accelerator.

CPU and GPU floating-point accumulation can differ slightly. Compare returned
neighbor identities and SQL ordering semantics, not byte-identical score text.

## 10. Batch and operate production workloads

The standalone server has finite admission and payload bounds by design:

| Limit | Default | Hard ceiling |
| --- | ---: | ---: |
| JSON request body | 32 MiB | 64 MiB |
| rows in one typed ingestion request | 10,000 | 1,000,000 |
| combined query rows from one `/v1/sql` request | 10,000 | 1,000,000 |
| structured vector-search result | 10 | 1,000 |

Configure the first three with `VECTORS_HTTP_MAX_JSON_BYTES`,
`VECTORS_HTTP_MAX_BULK_ROWS`, and `VECTORS_HTTP_MAX_RESPONSE_ROWS`. Values over
the hard ceilings make startup fail.

Advanced worker, connection, timeout, snapshot, and autosave settings are
listed under [Server configuration](../README.md#server-configuration).

For reliable ingestion:

1. Prefer the typed rows endpoint over building a very large SQL `INSERT`.
2. Begin with independently retryable batches, for example 1,000 rows, then
   measure throughput, encoded size, recovery, and memory on production-like
   hardware.
3. Give every row a stable primary key and use an explicit conflict policy so a
   client can safely retry after losing a response.
4. Keep a batch comfortably below both the HTTP body limit and the durable
   WAL's 64 MiB encoded-record limit. Row metadata also consumes space.
5. Checkpoint during a planned low-write window if a large catalog makes the
   writer pause visible; concurrent reads continue during checkpoint creation.

The process-wide database task limit defaults to available CPU parallelism.
When it is full, new database requests receive HTTP 503 with error code
`overloaded` and `Retry-After: 1`; retry with bounded exponential backoff and
jitter. Tune `VECTORS_MAX_CONCURRENT_DATABASE_TASKS` from observed CPU, memory,
latency, and rejection metrics rather than connection count alone. Writes are
serialized by the shared catalog even when reads run concurrently.

Use these operational endpoints:

- `/healthz` for process liveness;
- `/readyz` for catalog revision, task capacity, and configured request limits;
- `/metrics` for task utilization, rejection count, revision, and HTTP bounds.

## Troubleshooting

### `Address already in use`

Another process owns the port. Pick a different localhost port:

```sh
vectors-server --data-dir ./vectors-data --port 8081
```

### The shell stays at `...>`

The SQL statement is incomplete. Add the final semicolon or type `.cancel`.
Semicolons inside quoted strings and comments do not finish a statement.

### `dimension mismatch`

The inserted or query vector does not match `VECTOR(n)`. Inspect the table with
`.schema TABLE` or `GET /v1/tables/{table}/schema`, then send exactly `n`
finite numbers.

### Cosine distance or normalization rejects a vector

The vector has zero length. Store or query a non-zero embedding, or use an L2
metric when zero vectors are meaningful to the application.

### GPU execution is unavailable

Use `auto` for safe fallback or `cpu` for predictable placement. A source build
needs `--features gpu`; a compatible adapter and an eligible `VectorTopK` plan
are also required. Inspect the query with `EXPLAIN`.

### A SQL response is rejected as too large

Add `LIMIT`, page with a stable `ORDER BY` plus `LIMIT`/`OFFSET`, or raise
`VECTORS_HTTP_MAX_RESPONSE_ROWS` within its hard ceiling. The limit is shared by
all queries in one `/v1/sql` request.

### HTTP 413, `too_many_rows`, or a WAL-record size error

Split the import into smaller typed batches. Raising the JSON or row limit does
not raise the durable record-size boundary.

### HTTP 401 from `/v1`

Supply the exact bearer token configured in `VECTORS_API_TOKEN`. Health,
readiness, metrics, and the console itself remain public.

### HTTP 503 `overloaded`

Honor `Retry-After`, retry with backoff, and watch
`vectors_database_tasks_in_flight` plus
`vectors_database_tasks_rejected_total`. Increase capacity only after measuring
the process under representative load.

### The durable directory cannot be opened

Only one `vectors` process may own a data directory. Stop the other shell or
server, or select a different `--data-dir`. Do not delete `vectors.lock` while
another process is running.

## Where to go next

- Run [`examples/quickstart.sql`](../examples/quickstart.sql) for a repeatable
  shell lesson.
- Read the [architecture guide](ARCHITECTURE.md) for execution, concurrency,
  GPU, and recovery invariants.
- Use the [benchmark guide](BENCHMARKS.md) to measure exact search and durable
  ingestion honestly.
- Review the [roadmap](../ROADMAP.md) for ANN indexing, replication, and other
  planned boundaries.
- Read [security guidance](../SECURITY.md) before exposing a server outside a
  trusted local environment.
