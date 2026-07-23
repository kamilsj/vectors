# Changelog

Notable project changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and releases use
semantic versioning while the public API remains pre-1.0.

## Unreleased

## 0.4.0 - 2026-07-23

### Added

- `Database::query_intent` for validating and interpreting one read-only SQL
  query against the live catalog without executing it.
- Schema-aware output roles for identifier, content, attribute, embedding,
  similarity-score, and computed columns, including expansion of `SELECT *`.
- `POST /v1/sql/intent` with structured table, column, filter, ordering, limit,
  vector metric, dimensions, and optimized-plan metadata.
- An **Understand query** action in the web console that explains SQL intent and
  displays the role of every returned column before execution.
- Health metadata for the running version and storage mode; the web console now
  displays the real server version and durability mode instead of hard-coded
  placeholders.

### Changed

- Storage-lock and WAL-corruption failures now map to HTTP 500 rather than a
  client input error.

## 0.3.0 - 2026-07-23

### Added

- Directory-backed databases with checksummed write-ahead logging, synchronized
  commits, crash recovery, exclusive process locks, and automatic checkpoint
  compaction.
- `Database::open_persistent`, `Database::checkpoint`, and
  `Database::data_directory` for embedded durable storage.
- `--data-dir` support in both binaries and `.checkpoint` in the SQL shell.
- A reproducible durable-ingestion, recovery, and checkpoint benchmark.

### Changed

- Installers now start the server in durable WAL mode by default. Setting the
  legacy `VECTORS_SNAPSHOT` variable retains interval-based snapshot behavior.
- Snapshot format version 3 records the durable WAL sequence while remaining
  backward-compatible with versions 1 and 2.
- Every SQL write request and typed embedding batch is staged atomically and
  logged before it becomes visible.

## 0.2.1 - 2026-07-21

### Added

- `vectors-server --port` and `--bind` options for selecting a listen address
  without setting an environment variable.

### Changed

- Primary-key and unique-constraint checks use maintained internal key maps for
  inserts and idempotent conflict handling.
- Address conflicts report a recovery command with a suggested alternative port
  instead of first claiming that the server is listening.

## 0.2.0 - 2026-07-21

### Added

- Specialized `VectorTopK` execution for common exact vector-search queries.
- Parallel scoring with thread-local bounded heaps for large candidate sets.
- Bounded shared SQL AST caching for repeated queries across cloned handles.
- Typed atomic bulk insertion shared by the Rust API and HTTP ingestion route.
- Reproducible search and snapshot benchmark example.
- Architecture, performance, security, and roadmap documentation.
- GitHub issue templates, dependency updates, and tagged-release automation.
- Checksum-verifying Linux and Windows installers with optional automatic web
  console startup.

### Changed

- Snapshot vector I/O now uses reusable contiguous buffers and 1 MiB streams.
- Cosine similarity kernels use an unrolled, compiler-vectorizable loop.
- Structured JSON ingestion no longer serializes values into SQL and reparses
  them before insertion.
- Append-only insert batches extend scalar hash indexes incrementally instead
  of rebuilding buckets for every existing row.

## 0.1.0 - 2026-07-20

### Added

- In-memory SQL engine with relational and fixed-width `VECTOR(n)` values.
- Exact cosine, L2, squared-L2, and dot-product search.
- Scalar hash indexes and hybrid metadata filtering.
- Atomic versioned snapshots, autosave, and catalog revisions.
- Actix HTTP API, interactive shell, and built-in web console.
