# Roadmap

The goal is a dependable SQL-first vector engine, not a checklist of features.
Work is ordered by the amount of user value it unlocks without weakening query
correctness or recoverability.

## Now: make the exact engine dependable

- Keep optimized vector plans equivalent to the general SQL executor.
- Track query planning and snapshot performance with reproducible benchmarks.
- Add fuzz and property tests for expressions, vector kernels, and corrupted
  snapshot input.
- Maintain snapshot compatibility and corruption coverage across format
  versions 1 through 3.
- Improve query diagnostics with stable plan and timing metadata.
- Exercise WAL recovery with subprocess crash tests and storage fault injection.

Completion means the test corpus covers failure atomicity and persistence
boundaries, CI exercises supported platforms, and benchmark regressions can be
reproduced from a clean checkout.

Version 0.6 completed exact scalar-index coverage tracking, bounded HTTP
database-task admission, configurable server capacity, readiness metadata, and
initial Prometheus metrics. The next reliability work expands failure injection
and latency observability rather than weakening overload protection.

## Next: scale the working set

- Introduce a dense vector storage layout that avoids per-row enum traversal.
- Add an approximate-nearest-neighbor index, beginning with HNSW, while keeping
  exact search as the correctness oracle.
- Teach the planner to choose exact or ANN search from candidate count, filter
  selectivity, requested recall, and `LIMIT`.
- Persist vector indexes with versioning and corruption validation.
- Add prepared statements and typed parameters for repeated queries.

ANN support is complete only when index build cost, memory use, recall, filtered
search behavior, persistence, and concurrent reads are measured and documented.
SQL must expose whether a plan is exact or approximate.

## Later: durable service operation

- Compact checkpoints without blocking the query path for the full write.
- Add joins and subqueries needed for richer hybrid retrieval.
- Expand metrics with latency and result-size histograms; add request tracing,
  cancellation, and per-query CPU and memory limits.
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
