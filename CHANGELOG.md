# Changelog

Notable project changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and releases use
semantic versioning while the public API remains pre-1.0.

Pull requests record user-visible changes under **Unreleased**. During a
release, those entries move into a dated version section; the release workflow
uses that section as the curated introduction to the GitHub release notes.

## Unreleased

### Added

- Typed document fields for RAG collections, with required/unique constraints,
  maintained scalar indexes, and one shared visual field editor for collections
  and ordinary SQL tables. Declared metadata lives in canonical SQL columns.
- Two-table SQL `INNER JOIN` and `LEFT JOIN` on scalar equality, including
  vector ranking, aliases, filters, and bounded results. Joins reuse existing
  hash indexes or build a temporary lookup and stream matching row pairs.
- Named relationships between different tables through Data, the HTTP API,
  and synchronous/asynchronous Python clients. Links match compatible fields,
  create lookup indexes atomically, and open a ready-to-edit SQL join.

### Changed

- Metadata-only document edits reuse existing embeddings, chunk relationships,
  and keyword indexes, with zero embedding usage and revision protection.
- Data creation offers RAG documents or structured tables through the existing
  workflows; collection fields are editable alongside document text.

### Fixed

- Preserve the newly created table selection while the Data catalog refreshes.
- Keep row and relationship revisions consistent after the console's own
  writes, preserving drafts and conflict protection against external changes.

## 0.8.0 - 2026-09-20

### Added

- RAG relationship direction, exact-label, and minimum-weight controls in the
  HTTP API, Rust API, Python SDK, and console. Graph-derived hits retain
  snapshot-consistent discovery paths, including omitted bridge identifiers,
  with an expandable explanation in the console.
- Reproducible uv SDK development with a locked environment, package metadata
  and wheel/source-install checks, Python 3.10/3.14 CI, and a separate
  `python-v*` release workflow using PyPI trusted publishing.
- Installable Python SDK with synchronous/asynchronous clients, typed SQL and
  vector results, bounded generator-based imports with partial-progress errors,
  embedding and GraphRAG helpers, revision-checked relationship edits, and live
  server integration tests.
- Typed `$1`, `$2`, ... SQL parameters for the HTTP execution/intent endpoints
  and embedded Rust API, with safe value binding, bounded expansion, and
  existing atomic write, result-limit, and durable recovery guarantees.
- A Connections workspace to browse chunk relationships, inspect citations,
  preview and ingest documents, and manage directed links with revision checks.
- Provider-free focused graph exploration across page boundaries through
  `/neighborhood` and **Explore connections**, with root/depth markers,
  incoming/outgoing direction, relationship kind and weight filters, and
  bounded node/edge responses. Kind filtering is available through the API;
  console type checkboxes only hide drawn links.
- Hybrid RAG retrieval combines BM25 and vector ranks, graph context, optional
  Voyage cross-encoder reranking, and diversity-aware context budgets. A bounded
  chunk-generation-aware lexical cache avoids repeated document tokenization.
- Source-aware Unicode chunking with paragraph/sentence boundaries, overlap,
  Markdown heading context, exact source offsets, and a provider-free preview.
- SQL-visible GraphRAG collections with pinned embedding profiles, atomic
  document ingestion/replacement/deletion, adjacency and semantic relationships,
  custom SQL relationship labels, and bounded retrieval with source citations.
- `vectors update` checks and installs newer stable releases; opt-in watch mode
  checks periodically using verified installers, managed restarts, and rollback.
- OpenAI and Voyage AI text embeddings with server-side keys, persistent
  non-secret settings, bounded batches, timeouts, and provider-response checks.
- A Data admin workspace and typed administrative API for paged browsing,
  table creation, document embedding, and individual row edits/deletes guarded
  by atomic catalog revision checks.

- Browser regression tests for API connection recovery, stale responses,
  duplicate submissions, vector validation, and large result tables.

### Changed

- Release builds verify the embedded GraphRAG console, live API, and durable
  restart before publication and after installation on every supported platform.
  Browser regression checks also gate publication.
- BM25 index construction borrows lowercase ASCII tokens and reuses vocabulary
  keys, reducing repeated-word allocations while preserving Unicode scoring
  and bounded-cache fallback behavior.
- Focused graph neighborhoods reuse maintained chunk/document ID indexes
  instead of allocating full-collection lookup maps for every request. A
  correctness-checked benchmark measures fixed neighborhoods across corpus sizes.
- Hybrid `/retrieve` graph expansion merges all frontier proposals before
  applying each hop's bounded beam, combining path strength with query fit and
  retaining useful traversal through bridges omitted from final context.
  `/search` retains its existing vector-seed traversal.
- Keyword indexes remain cached after relationship edits and unrelated writes;
  chunk storage changes and reopen invalidate them. Capacity-aware checks bound
  retained indexes, and precomputed BM25 length factors preserve score arithmetic.
- Embedding requests use conservative per-input and per-model batch budgets,
  explicit retrieval roles, and profile checks before provider calls. Graph
  ingestion skips unchanged documents and validates capacity before embedding.
- Structured vector searches bypass SQL text construction and parsing while
  sharing SQL's indexes, exact top-k execution, and CPU/GPU paths. Bulk imports
  reuse column maps and move text/vector buffers; SQL and search responses are
  encoded directly on admitted database workers without intermediate JSON trees.
- Remove repeated column-name allocations from SQL projections and residual
  vector-search filters; add a reproducible narrow/wide table benchmark.
- Simplify navigation into Search, Connections, Data, SQL, and Settings. Text search is the
  default; raw-vector search and SQL remain available alongside browser
  preferences and effective server settings.

- Web results render one page at a time and expand full vectors or long values
  on demand. Read-only SQL avoids redundant catalog/schema requests.
- Embedded console assets negotiate compression and use content-based cache
  validators, allowing unchanged reloads to return empty 304 responses.

### Fixed

- Small browser retrieval budgets now suggest fewer starting passages, leaving
  room for connected context; explicitly chosen seed counts remain adjustable.
- A stronger graph route now updates path evidence and depth together, so the
  displayed connection chain describes the retained retrieval route.
- Duplicate relationship triples inserted through SQL use their strongest
  weight consistently in RAG paths and returned graph edges.
- Accept the documented SQL `<=>` cosine operator in projections, predicates,
  grouped expressions, and optimized vector top-k plans.
- Rank API searches by the computed score when a table contains a `distance`
  column, reject duplicate selected columns, and honor configured response-row
  limits. Reject schema changes atomically during typed imports so reordered
  columns cannot silently receive the wrong values.
- Preserve durable database revisions across WAL recovery and checkpoints so
  stale administrative edits and deletes stay rejected after a restart.
- Exclude local output and browser-test artifacts from published crate packages.
- Keep slow responses from replacing newer table selections or API-token
  sessions, prevent duplicate SQL submissions from keyboard shortcuts, and
  invalidate cached schemas on refresh. Connection failures preserve the
  editor and table list; query errors no longer report a lost connection.
- Validate vector dimensions, empty components, and numeric/boolean filters
  before sending a search. Token entry also works when browser storage is
  disabled and submits with Enter.
- Keep query controls and summary cards inside tablet layouts; preserve SQL
  line alignment when statements extend beyond the editor width.
- Invoke the Windows one-line installer as a script block so its PowerShell
  confirmation and preview options initialize correctly. Smoke tests now run
  the documented download commands, including Windows PowerShell 5.1.
- Keep macOS installer smoke tests successful when their test server finishes
  its requested graceful shutdown.

## 0.7.0 - 2026-09-17

### Added

- A CPU scan-layout benchmark with deterministic typed ingestion, exact-result
  checks, latency percentiles, and controls for dimensions, batch size, and
  Rayon thread count.
- Optional wgpu exact-scan acceleration behind the `gpu` Cargo feature, with
  Vulkan, DirectX 12, and Metal backends, lazy adapter initialization, and a
  bounded generation-aware dense-column cache.
- Public `ComputeConfig` and `ComputeDevice` settings for embedded users, plus
  `--compute`, `VECTORS_COMPUTE_DEVICE`, `VECTORS_GPU_MIN_ELEMENTS`, and
  `VECTORS_GPU_CACHE_BYTES` controls for the standalone server.
- CPU, automatic, and required-GPU modes in the vector-search benchmark, with
  neighbor primary-key comparison against the general SQL executor.
- Public HTTP `RequestLimits` plus standalone-server controls for JSON body
  bytes, typed bulk rows, and SQL response rows. Defaults are 32 MiB, 10,000
  rows, and 10,000 rows respectively, with validated deployment ceilings.
- A guided `.tutorial` in the interactive shell, a repeatable executable SQL
  quickstart, and a full cross-platform tutorial covering commands, APIs,
  persistence, vector operators, compute policy, and troubleshooting.
- First-run web-console onboarding with guided setup/search steps, a searchable
  mental model of the workflow, keyboard-accessible help, and an expandable
  command reference.
- Native release archives and installer coverage for Linux x86-64/ARM64,
  macOS Intel/Apple silicon, and Windows x86-64, with network-free target and
  installation-plan inspection modes.
- A cross-platform installation guide covering fixed-version installs,
  upgrades, verification, platform paths, safe removal, and troubleshooting.

### Changed

- Installation quickstarts now use one command per platform and direct installer
  links, with custom-port and install-only examples and an optional
  download-and-review path.
- Exact CPU searches distribute full scans across balanced row ranges, allowing
  large ingestion slabs to use multiple cores. Indexed and residual-filter
  scans amortize scheduling and heap merging across dimension-aware batches;
  single-thread pools use the sequential path.
- Vector columns now maintain append-only contiguous `f32` slabs with cached
  norms, compact null-presence metadata, and shared row views. Exact top-k scans
  read this dense layout without traversing relational values for each
  candidate.
- Dense append and rebuild paths split vector payloads around an 8 MiB slab
  target and use a sparse lookup entry per 4,096 rows. Snapshot loading rebuilds
  dense columns incrementally in bounded row/vector batches.
- Unfiltered CPU top-k scans walk dense slabs directly and parallelize across
  fragmented slabs; Euclidean ordering stays in squared-distance space and
  takes square roots only for rows that survive top-k.
- SQL response budgets are pushed into execution so unordered scans stop at an
  overflow sentinel and ordered top-k plans retain bounded heaps instead of
  constructing an arbitrarily large final response before rejection.
- Common append-only single-statement durable `INSERT` operations validate and
  WAL-sync an append delta before applying it to the live table, avoiding a full
  catalog clone; multi-statement writes retain staged atomic execution.
- Checkpoint compaction now holds a shared catalog read guard through snapshot
  synchronization and WAL reset, allowing concurrent reads and vector searches
  to continue while writes wait for the coherent durable boundary.
- Public snapshot replacement and managed checkpoint replacement share one
  serialization guard, preventing an older user save from racing a WAL reset;
  inserts also enforce the snapshot format's 10,000,000-row table ceiling
  before a commit can make future compaction impossible.
- Automatic compute selection retains the Rayon CPU path for small or
  incompatible scans and falls back safely when no supported GPU is available;
  required-GPU mode reports an explicit availability error.
- GPU columns are split into device-sized shards when necessary, indexed inputs
  and readback are bounded, and exact scores stream directly into the CPU top-k
  heap without a request-sized score vector.
- Tagged release binaries are built with GPU support while the library's
  default feature set remains lean. CI compiles, lints, and tests all features.
- Shell and server help now group commands, explain durable startup and compute
  options, and provide copy-ready examples; installer startup messages lead
  directly to the terminal or web tutorial.
- Installers now verify checksums and binary versions, use transactional binary
  replacement, preserve database files, remember managed bind/storage settings,
  and restore the previous runtime and configuration when an upgrade cannot
  start successfully.
- Installer-managed servers use a private cooperative-shutdown request so
  graceful Actix teardown completes the final WAL checkpoint or legacy snapshot
  before an upgrade proceeds.
- Release automation publishes installer checksums and build-provenance
  attestations, then installs and health-checks every supported native target.
- Pull requests must record release-note intent, and tagged releases validate
  and publish the matching curated changelog section alongside categorized
  GitHub-generated details.

### Fixed

- Publish the macOS Intel/Apple silicon and Linux ARM64 installers and native
  archives together, so the documented latest-release command works on those
  platforms. Release checks now execute the downloaded installer through the
  same one-line entry points used in the documentation.
- Cancelled HTTP requests retain their database capacity slots until queued or
  running work finishes, preventing disconnects and request timeouts from
  bypassing overload protection or undercounting in-flight database work.

## 0.6.0 - 2026-07-24

### Added

- Configurable Actix worker, blocking-thread, connection, database-task,
  keep-alive, client-header-timeout, and graceful-shutdown limits through the
  Rust API and standalone-server environment variables.
- A global database-task capacity guard that returns HTTP 503 with
  `Retry-After: 1` instead of allowing an unbounded blocking-work queue.
- Public `/readyz` readiness metadata and Prometheus-compatible `/metrics` for
  catalog revision, database work in flight, configured capacity, and overload
  rejections.

### Changed

- Exact scalar-index coverage is now tracked separately from partial candidate
  pruning. Fully covered predicates skip redundant expression evaluation in the
  general, aggregate, and optimized vector executors while residual predicates
  retain the original validation and evaluation path.
- The standalone server now uses explicit production-oriented Actix defaults
  and enables `TCP_NODELAY` for request latency.
- The reproducible 20,000-row, 64-dimension exact hybrid-search workload is
  32.4% faster than 0.5.0 on the reference machine, with identical neighbors.

## 0.5.0 - 2026-07-24

### Added

- Declared result-column types throughout the general, aggregate, and optimized
  vector execution paths, including correct metadata for empty and all-`NULL`
  result sets.
- A `schema` array on HTTP query responses while retaining the existing
  `columns` and `rows` fields for compatibility.
- Aggregate-aware SQL intent metadata for `DISTINCT`, `GROUP BY`, `HAVING`,
  aggregate output roles, and statically inferred computed-column types.

### Changed

- Expression types are validated from the AST before row scanning, so invalid
  arithmetic, predicates, function arguments, vector dimensions, and sort keys
  fail consistently even for empty tables; aggregates are also rejected in
  illegal `WHERE` and `GROUP BY` positions before execution.
- The web console displays declared result types and richer aggregate intent
  details instead of inferring meaning from returned values.

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
