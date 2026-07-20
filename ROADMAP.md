# Roadmap

The goal is a dependable SQL-first vector engine, not a checklist of features.
Work is ordered by the amount of user value it unlocks without weakening query
correctness or recoverability.

## Now: make the exact engine dependable

- Keep optimized vector plans equivalent to the general SQL executor.
- Track query planning and snapshot performance with reproducible benchmarks.
- Add fuzz and property tests for expressions, vector kernels, and corrupted
  snapshot input.
- Define a snapshot compatibility policy before changing format version 2.
- Improve query diagnostics with stable plan and timing metadata.

Completion means the test corpus covers failure atomicity and persistence
boundaries, CI exercises supported platforms, and benchmark regressions can be
reproduced from a clean checkout.

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

- Add a write-ahead log and recovery tests for interrupted writes.
- Compact checkpoints without blocking the query path for the full write.
- Add joins and subqueries needed for richer hybrid retrieval.
- Expose structured metrics, request tracing, cancellation, and resource limits.
- Design replication only after the single-node durability contract is stable.

Durability work is complete when automated crash tests demonstrate the stated
recovery guarantee. Replication will not substitute for local correctness.

## How priorities change

Open an issue with a concrete workload, data shape, query, and success measure.
Measured use cases carry more weight than broad feature requests. Large design
changes should include alternatives considered and compatibility implications.
