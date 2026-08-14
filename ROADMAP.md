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

## Next: scale the working set

- Add an approximate-nearest-neighbor index, beginning with HNSW, while keeping
  exact search as the correctness oracle.
- Teach the planner to choose exact or ANN search from candidate count, filter
  selectivity, requested recall, and `LIMIT`.
- Persist vector indexes with versioning and corruption validation.
- Add prepared statements and typed parameters for repeated queries.
- Add explicit host-memory accounting, configurable table/query budgets, and
  backpressure for very large ingestion requests.
- Add streaming ingestion and partitioned index construction so input size does
  not need to be represented as one request or one prospective catalog clone.

ANN support is complete only when index build cost, memory use, recall, filtered
search behavior, persistence, and concurrent reads are measured and documented.
SQL must expose whether a plan is exact or approximate. Large-dataset support
also requires enforced resource budgets and recovery tests; accepting a larger
HTTP body alone does not satisfy it.

## Later: durable service operation

- Add background checkpoint/WAL rotation so snapshot I/O no longer excludes
  writers; preserve the current concurrent-read and crash-ordering guarantees.
- Add joins and subqueries needed for richer hybrid retrieval.
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
