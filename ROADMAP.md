# Roadmap

The goal is a dependable SQL-first vector engine, not a checklist of features.
Work is ordered by the amount of user value it unlocks without weakening query
correctness or recoverability.

## Now: make the exact engine dependable

- Keep optimized vector plans equivalent to the general SQL executor.
- Track query planning and snapshot performance with reproducible benchmarks.
- Add fuzz and property tests for expressions, vector kernels, and corrupted
  snapshot input.
- Exercise dense-column rebuilds and CPU/GPU result equivalence across nullable
  vectors, mutations, recovery, device loss, and configured memory limits.
- Maintain snapshot compatibility and corruption coverage across format
  versions 1 through 3.
- Improve query diagnostics with stable plan and timing metadata.
- Exercise WAL recovery with subprocess crash tests and storage fault injection.
- Record CPU/GPU crossover data on named adapters before changing automatic
  compute thresholds or publishing accelerator performance claims.

Completion means the test corpus covers failure atomicity and persistence
boundaries, CI exercises supported platforms, and benchmark regressions can be
reproduced from a clean checkout.

Version 0.6 completed exact scalar-index coverage tracking, bounded HTTP
database-task admission, configurable server capacity, readiness metadata, and
initial Prometheus metrics. The next reliability work expands failure injection
and latency observability rather than weakening overload protection.

Current unreleased work adds dense append-only vector slabs with cached norms
and presence metadata, plus optional exact wgpu scans with bounded device
caching and CPU fallback. These are scan-engine improvements; they do not remove
the requirement that the active catalog fit in host memory.

CPU scan scheduling now divides work independently of ingestion slab boundaries
and uses dimension-aware task sizes. A correctness-checked layout/thread
benchmark and Apple M4 Max measurements are recorded in `docs/BENCHMARKS.md`;
broader CPU and GPU measurements remain necessary before changing device policy.

## Next: scale the working set

- Use the [ten-million-document plan](docs/SCALING.md) to track chunk counts,
  storage budgets, semantic-graph growth and acceptance tests. This is a target;
  the current GraphRAG limit remains 10,000 chunks per collection.
- Introduce disk-resident segments and bounded caches for text, vectors,
  lexical postings and graph adjacency, with stable IDs and bounded recovery.
- Add an approximate-nearest-neighbor index, beginning with HNSW, while keeping
  exact search as the correctness oracle. Compare disk/memory costs and
  filtered recall with inverted-file alternatives before committing to the
  large-corpus storage design.
- Teach the planner to choose exact or ANN search from candidate count, filter
  selectivity, requested recall, and `LIMIT`.
- Persist vector indexes with versioning and corruption validation.
- Add reusable prepared statements and parameter-aware plan reuse. Typed `$1`
  value binding is available in the HTTP API, Rust API, and Python SDK; it still
  parses the bound SQL and does not yet reuse plans across parameter values.
- Add explicit host-memory accounting, configurable table/query budgets, and
  backpressure for very large ingestion requests.
- Add streaming ingestion and partitioned index construction so input size does
  not need to be represented as one request or one memory-resident catalog.

The current foundations share unchanged catalog tables, append new graph
documents atomically, reuse validated profile generations, and avoid full
chunk masks for selective retrieval. These reduce local work without changing
the memory-resident storage model, exact search or collection limits.

ANN support is complete only when index build cost, memory use, recall, filtered
search behavior, persistence, and concurrent reads are measured and documented.
SQL must expose whether a plan is exact or approximate. Large-dataset support
also requires enforced resource budgets and recovery tests; accepting a larger
HTTP body alone does not satisfy it.

## Later: durable service operation

- Add background checkpoint/WAL rotation so snapshot I/O no longer excludes
  writers; preserve the current concurrent-read and crash-ordering guarantees.
- Extend the implemented streaming INNER/LEFT join chains with join-order
  planning, aggregate joins, and subqueries for richer hybrid retrieval.
- Extend typed document filtering to explicit linked-record predicates in
  GraphRAG; named cross-table links currently participate through SQL joins.
- Expand metrics with latency and result-size histograms; add request tracing,
  cancellation, and per-query CPU and memory limits.
- Move eligible GPU top-k reduction onto the device only when benchmarks show
  that avoiding full score readback improves end-to-end latency without
  weakening deterministic result checks.
- Design replication only after the single-node durability contract is stable.

Durability work is complete when automated crash tests demonstrate the stated
recovery guarantee. Replication will not substitute for local correctness.

The first durability foundation shipped in 0.3: fsynced checksummed WAL records,
typed-ingestion logging, exclusive directory locks, torn-tail recovery, and
versioned checkpoint compaction. The remaining work focuses on fault injection,
background checkpoint rotation, and operational metrics rather than changing
the acknowledged-write contract.

## How priorities change

Open an issue with a concrete workload, data shape, query, and success measure.
Measured use cases carry more weight than broad feature requests. Large design
changes should include alternatives considered and compatibility implications.
