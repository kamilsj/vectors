# Chat indexing and retrieval at scale

Research and code review: 2026-10-09. Implementation baseline: `a71b4e7`.
The conclusions below are engineering choices for Vectors/Saywit, not claims
that one algorithm is universally best or that a ten-million-document workload
has been validated.

## Findings from primary sources

| Pattern | Evidence | Application to Vectors and Saywit |
| --- | --- | --- |
| Filter-aware planning | [Qdrant indexing](https://qdrant.tech/documentation/manage-data/indexing/) uses payload indexes to estimate cardinality and chooses a full scan for sufficiently small filtered sets. Filtered graph traversal addresses intermediate selectivities. | Keep exact scoring for small permitted chat scopes. Any future ANN planner must measure the eligible set and apply authorization-derived filters before admission to top-k. |
| Continuous index maintenance | [FreshDiskANN](https://arxiv.org/abs/2105.09613) studies streaming insert/delete/search. [IP-DiskANN](https://arxiv.org/abs/2502.13826) studies in-place graph updates and recall stability over lengthy update sequences. | A static ANN benchmark is insufficient for chat. Evaluate recall after edits, deletions and long churn sequences before selecting an ANN implementation. |
| Explicit visibility guarantees | [Elasticsearch refresh semantics](https://www.elastic.co/docs/reference/elasticsearch/rest-apis/refresh-parameter) distinguish a write from its visibility to search and explain the cost of frequent refreshes and small segments. | Preserve Vectors' acknowledged-write visibility and bounded batching. Measure publication latency, not just enqueue throughput. |
| Efficient grouped deletion | [RocksDB DeleteRange](https://rocksdb.org/blog/2018/11/21/delete-range.html) explains why scan-and-delete and rewriting unaffected data are costly, and why deletion needs atomicity and recovery guarantees. | Turn identity is an existing indexed grouping key. Use it to find obsolete units and retain unaffected vector storage. This is an adaptation of the principle, not a RocksDB tombstone implementation. |
| Conditional publication | [PostgreSQL ON CONFLICT](https://www.postgresql.org/docs/current/sql-insert.html) provides atomic upserts and a condition evaluated against the conflicting row. | Future per-turn revision checks should share the publication transaction, so a delayed job cannot overwrite newer content. Global catalog revisions are too broad for independent chat workers. |
| Multiple retrieval signals | [Qdrant hybrid queries](https://qdrant.tech/documentation/search/hybrid-queries/) combines independent rankings with reciprocal rank fusion and supports subsequent scoring stages. | Evaluate lexical matches plus dense retrieval for names, identifiers and paraphrases, followed by bounded reranking. Vectors GraphRAG already has hybrid fusion; Saywit's current SQL adapter uses dense cosine only. |
| Evaluate conversational memory | [LongMemEval](https://arxiv.org/abs/2410.10813) separates extraction, multi-session reasoning, temporal reasoning, knowledge updates and abstention. | Add these categories to a labeled Saywit evaluation set. Exact vector ranking tests establish engine correctness, not the quality of answers or memory recall. |

The [Microsoft DiskANN library](https://github.com/microsoft/DiskANN) exposes
streaming updates, quantizers, memory tiers and filter hooks. It is a candidate
for a separately measured broad-scope search backend. Its upstream benchmarks
do not establish Vectors' capacity or recall, and it has not been integrated here.

## Improvement implemented after this review

Saywit's inspected `semantic/indexer.py` deletes existing units by `turn_id`
before inserting each turn. For a new turn, that deletion usually finds no rows.
The previous Vectors path nevertheless detached the table, scanned all rows and
rebuilt its scalar and dense vector indexes.

The new path prepares the deletion before taking mutable ownership. Predicates
fully covered by scalar indexes use their matching row IDs. A no-match delete
leaves table identity, vectors, caches, revision and WAL untouched. Predicates
with residual expressions retain full evaluation so indexing cannot suppress
an expression error that previously rejected the operation.

For actual deletion, retained rows keep their source order. Unaffected vector
slabs are shared at their new row offsets; only slabs containing deleted rows
are repacked. Scalar maps retain their keys and allocations while remapping
surviving row positions. A single
durable DELETE is validated, logged and synchronized before it becomes visible,
without staging another whole-table copy. Multi-statement transactions and
revision-guarded admin operations retain their existing rollback path.

Tests cover NULLs, boolean predicates, expression errors, no-op revisions/WAL,
snapshot isolation, multiple vector-column layouts, slab lookup boundaries,
subsequent upserts, exact search, checkpoint recovery and a forced server kill
after an acknowledged deletion. The [cleanup benchmark](BENCHMARKS.md#chat-turn-cleanup)
measures missing turns, real deletions and the existing delete-then-upsert cycle.

## Next stages and acceptance criteria

1. **Atomic turn publication.** Add a per-turn generation/revision and an atomic
   replacement operation. Reject stale publications and retain deletion versions
   long enough to prevent delayed workers from restoring deleted units. Test
   reverse job completion, shortening a turn, regrouping, removed access, timeout
   retries and crashes between vector and source-record writes. The current
   separate DELETE and insert requests still permit a temporary retrieval gap.
2. **Segmented storage and bounded cleanup.** Introduce stable row handles before
   adding tombstones or background compaction. Benchmark retained memory, scalar
   index maintenance and checkpoint pauses under sustained churn. Current real
   deletions still compact row metadata and visit scalar-index entries; those
   costs grow with table size even when only one turn is removed.
3. **Optional approximate search.** Compare a filtered graph index and a
   disk-backed approach against the exact executor. Measure recall@k, p95/p99,
   resident memory, recovery time and filter selectivity at 100k, 1m and 10m
   synthetic units under concurrent queries and writes. Select the crossover
   from those measurements; retain exact search for small scopes and evaluation.
4. **Conversation quality evaluation.** Use source-labeled paraphrases, exact
   identifiers, follow-ups, corrected facts, time-qualified questions and
   unanswerable questions. Measure source recall, obsolete-source rate, citation
   support and abstention separately from latency. Include adversarial chat and
   private-message scope tests before any quality experiment.

These are proposed stages, not delivered functionality. Today the store is
memory-resident, writes are serialized, and checkpoints write the catalog.
Regular SQL tables have a 10,000,000-row safety ceiling subject to memory;
GraphRAG collections retain a 10,000-chunk limit. Deletion visibility is not a
secure-erasure guarantee for snapshots, backups or the pre-checkpoint WAL.
