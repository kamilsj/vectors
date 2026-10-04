# Scaling RAG to ten million documents

Ten million **documents** is a workload target, not a supported capacity claim
for the current engine. Chunk count, vector dimensions, graph degree, update
rate and concurrent queries determine the actual workload. A benchmark with ten
million SQL rows would not validate ten million multi-chunk documents.

## Current boundary

The active database is memory-resident. Graph collections enforce 10,000 chunks,
340,000 directed edges, 33,554,432 vector elements and 64 MiB of stored text.
The underlying SQL table limit is ten million rows. Vector retrieval and
automatic semantic linking are exact scans. There is no persisted approximate
nearest-neighbor (ANN) index, distributed coordinator or replica protocol.

These limits remain enforced. Increasing a constant would not make ingestion,
retrieval, recovery or memory usage safe at the requested scale.

The current scaling work removes avoidable costs within that boundary:

- Catalog snapshots share unchanged tables; a mutation copies a table only
  when another snapshot still owns it.
- New graph documents use prepared append operations committed together after
  their WAL record, while replacement and orphan repair retain staged writes.
- Selective document filters use the chunk document-ID index. Graph path state
  follows visited candidates, similarity memoization is bounded by the query
  budget, and selective keyword scoring avoids collection-sized score arrays.
- Successful embedding-profile checks are reused for an unchanged chunk-table
  generation. SQL writes, append, deletion, reload and configuration changes
  cannot reuse a stale successful check.

These changes do not introduce ANN, disk-resident retrieval, sharding or high
availability. See [measured benchmarks](BENCHMARKS.md) for tested workloads.

## Size the corpus before choosing the deployment

An illustrative target uses ten million documents, ten chunks per document,
1,536-dimensional float32 embeddings, 2,048 bytes of text per chunk and sixteen
directed semantic edges per chunk:

| Component | Calculation | Payload for one copy |
| --- | --- | ---: |
| Chunks | 10,000,000 × 10 | 100,000,000 |
| Embeddings | chunks × 1,536 × 4 bytes | 614.4 GB |
| Chunk text | chunks × 2,048 bytes | 204.8 GB |
| Compact edge records | chunks × 16 × an assumed 32 bytes | 51.2 GB |
| Payload subtotal | embeddings + chunk text + edge records | 870.4 GB |

GB here means 1,000,000,000 bytes. The edge format is a planning assumption,
not the current in-memory representation. This subtotal excludes source PDFs,
document metadata, duplicated context text, vector and lexical indexes, ID
dictionaries, allocator overhead, write buffers, caches, compaction space and
replicas. Three copies of the embedding payload alone require 1.8432 TB.
Document and chunk counts must therefore be reported separately in every test.

## Architecture milestones

### 1. Persisted segments and bounded recovery

Move chunk text, vectors, postings and adjacency lists into versioned segments
with explicit cache budgets. Keep stable document/chunk identifiers independent
of physical row positions. Append WAL records and publish segment manifests
atomically; compact in the background without blocking every writer. Stream
recovery rather than materializing all pending WAL records. Preserve citation
versions until readers using the old segment generation finish.

Acceptance: crash injection at every commit boundary, old-format migration,
bounded-memory restart and mixed read/write tests. A successful snapshot save
alone does not establish bounded recovery or uninterrupted writes.

### 2. ANN candidates with measured recall

Introduce an optional persisted vector index with exact search retained as a
reference. Compare memory, disk reads, build/update cost, filtered recall and
tail latency before selecting an index design. An IVF index can place inverted
lists on disk; graph-based indexes need their own storage and cache design.
See the [Faiss on-disk index documentation](https://github.com/facebookresearch/faiss/wiki/Indexes-that-do-not-fit-in-RAM).

Candidate selection must enforce tenant/access constraints before passages
become eligible for reranking or answers. Metadata filtering is currently a
query selector, not an authorization boundary. Evaluate selective filters and
rare entities against exact results; a fast unfiltered ANN benchmark is not
sufficient.

Acceptance: recall@10 and recall@100 against exact search on representative
query subsets, separate quality/latency curves for each filter selectivity,
persist/reopen parity, deletion correctness and bounded index-build memory.

### 3. Incremental semantic graph construction

Generate a bounded candidate neighborhood through the vector index, then score
only those candidates. Store relationship type, direction, weight, source
version and provenance. Limit outgoing degree and traversal work independently;
a few high-degree nodes must not create unbounded request work. Updating a
document must retire stale links and citations atomically, with a resumable
queue for recomputing affected neighbors.

Acceptance: deterministic stable-ID behavior, replacement/deletion repair,
degree and visited-edge budgets, disconnected-corpus tests, and measured
semantic-link precision. ANN navigation links and semantic evidence links have
different purposes and must not be presented as interchangeable evidence.

### 4. Partitioning and replication

Partition by stable document/tenant identity, with explicit handling of very
large tenants. Plan chunk placement, shard splits and cross-shard edges before
defining a public distributed API. Query routing needs bounded fanout, global
candidate merging, cancellation/deadline propagation and observable partial
failure. Replication needs a defined consistency model, failover and tested
restore procedures. Existing distributed vector systems illustrate why shard
movement and index rebuilding are separate operational costs; see
[Qdrant distributed deployment](https://qdrant.tech/documentation/scaling/distributed_deployment/).

Acceptance: node loss during ingestion and queries, replica lag, rolling
upgrade, shard rebalance under load and recovery from a complete backup. A
collection-per-server demo is not a validated distributed database.

## Validation ladder

1. **Current engine:** reproduce the published bounded-corpus benchmarks,
   mutation/recovery tests and Playground question-set evaluation.
2. **After segment and ANN milestones:** 100,000 then one million chunks,
   including realistic dimensions, text lengths, duplicates and filters.
3. **After partitioning:** ten million documents at the measured chunk-count
   distribution, followed by sustained ingestion, updates and concurrent reads.

For every step, retain the fixture seed, source revision, hardware, worker
counts, durability policy, cache state, raw timings and exact-reference results.
Report ingestion documents/s and chunks/s separately; include p50/p95/p99
retrieval latency, resident memory, disk space/read amplification, index build
time, restart time, ANN recall and graph-evidence quality. Run a soak test with
updates and deletions; a short read-only peak is insufficient.

Use a versioned, reviewed question set with expected citations, difficult
near-matches, multi-document questions, unanswerable questions and access-scope
cases. Playground evaluation measures source retrieval against supplied labels;
answer faithfulness and citation correctness require separate evaluation.
Latency and quality acceptance thresholds must be agreed for the deployment,
not inferred from a local synthetic benchmark.
