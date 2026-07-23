# Benchmarks

Performance work in `vectors` starts with a reproducible query and a correctness
check. The repository benchmark compares execution and parsing paths inside
this project; it is not presented as a comparison with another database.

## Run the benchmark

```sh
cargo run --release --example benchmark_vector_search
```

The benchmark:

1. creates a table with relational metadata and fixed-width vectors;
2. inserts deterministic data through SQL;
3. builds a scalar hash index for the filter;
4. verifies that `VectorTopK` and the general executor return the same rows;
5. times cached and uncached parsing of the same `VectorTopK` query;
6. times the general executor; and
7. saves and reloads a snapshot.

The generated snapshot is removed after the run. No network service is involved.

## Workload controls

Environment variables make the data shape repeatable:

| Variable | Search default | Ingestion default | Meaning |
| --- | ---: | ---: | --- |
| `VECTORS_BENCH_ROWS` | `20000` | `1000` | Generated rows or rows per batch |
| `VECTORS_BENCH_DIMENSIONS` | `64` | `64` | Dimensions per vector |
| `VECTORS_BENCH_ITERATIONS` | `8` | `10` | Timed repetitions |
| `VECTORS_BENCH_EXISTING_ROWS` | — | `20000` | Existing rows for the indexed-append case |

PowerShell example:

```powershell
$env:VECTORS_BENCH_ROWS = "100000"
$env:VECTORS_BENCH_DIMENSIONS = "384"
$env:VECTORS_BENCH_ITERATIONS = "20"
cargo run --release --example benchmark_vector_search
```

## Reference result

This result is the median of three local benchmark processes recorded on
2026-07-21, with 20 timed iterations per process. It should not be used to claim
a ranking against other databases.

| Item | Value |
| --- | --- |
| CPU | Intel Core i9-14900KS |
| Memory | 128 GiB |
| OS/toolchain | Windows x86-64 MSVC, Rust 1.96.1 |
| Dataset | 10,000 rows, 64 dimensions, 50% scalar-filter selectivity |
| Query | cosine distance, exact top 20 |
| Cached optimized SQL | 0.49 ms average |
| Uncached optimized SQL | 0.58 ms average |
| Parse-cache speedup | 1.20x |
| General SQL | 12.90 ms average |
| In-engine top-k speedup | 26.2x |
| Snapshot | 2.95 MiB; 19.5 ms save; 11.2 ms load |

The optimized query exercises hash-index pruning, one-time query-vector
evaluation, direct distance scoring, bounded heaps, and deferred projection.
Cache-miss queries add unique trailing comments so their ASTs are parsed again
without changing the plan. The general comparison query adds an arithmetic
projection to select the generic SQL executor while keeping the result set
equivalent.

## Indexed-filter optimization: 0.5.0 to 0.6.0

Version 0.6.0 records whether a scalar hash index covers the complete predicate.
When it does, both execution paths use the index result directly instead of
evaluating the same equality expression once more for every candidate. Partial
`AND`/`OR` coverage still evaluates the full predicate for every candidate.

The following controlled A/B result was recorded on the reference machine on
2026-07-24. Both revisions were built in release mode with the same locked
dependencies and Rust toolchain. Each process created 20,000 deterministic rows
with 64 dimensions, selected 10,000 candidates through a scalar hash index, and
ran 100 cosine top-20 queries. The table reports the median of three process
averages.

| Revision/path | Process averages | Median |
| --- | --- | ---: |
| 0.5.0 cached `VectorTopK` | 0.697 ms, 0.631 ms, 0.734 ms | 0.697 ms |
| 0.6.0 cached `VectorTopK` | 0.452 ms, 0.471 ms, 0.477 ms | 0.471 ms |
| 0.5.0 general SQL | 24.508 ms, 23.392 ms, 24.987 ms | 24.508 ms |
| 0.6.0 general SQL | 20.307 ms, 21.920 ms, 21.065 ms | 21.065 ms |

The exact fast path improved by 32.4% and the general path by 14.0%. The harness
compared all returned neighbors before timing. This is intentionally not a
Meilisearch comparison: Meilisearch uses the approximate-nearest-neighbor
[Arroy](https://github.com/meilisearch/arroy) index, while `vectors` 0.6.0
performs exact search. A useful comparison must match dataset, hardware,
filtering, concurrency, durability, recall, and end-to-end client overhead.

## Interpreting results

- Use a release build. Debug timings are not meaningful here.
- Run enough iterations to reduce scheduler noise and report the median of
  several process runs when publishing results.
- State whether the measured time includes ingestion, parsing, persistence, or
  network overhead.
- Keep dimensions, candidate count, filter selectivity, metric, and `LIMIT`
  visible. Each changes the cost substantially.
- Check results, not only elapsed time. A faster query returning different
  neighbors is a bug.

Cross-database comparisons require equivalent durability, exact-versus-ANN
behavior, index build time, recall, hardware, and client overhead. Add such a
benchmark only when its harness and raw results can be reviewed in the
repository.

## Typed ingestion benchmark

```sh
cargo run --release --example benchmark_ingestion
```

This benchmark prepares equivalent typed rows and SQL text before timing, then
loads each into fresh databases. It isolates the engine insertion boundary: it
does not include JSON decoding, request transport, or input generation. Both
paths use the same validation, uniqueness, mutation, and revision code, and the
harness verifies the affected row count.

The median of three processes on the reference machine, with ten 1,000-row ×
64-dimension batches per process, was 0.23 ms for typed insertion and 51.67 ms
for SQL literal parsing plus insertion—roughly 230x at this boundary. This is
not an end-to-end HTTP throughput claim. Use an HTTP load generator when
measuring an application deployment.

The same harness appends 1,000 rows to a 20,000-row table with a primary key and
a scalar hash index. Incremental scalar and unique-key maintenance reduced this
local case from a 1.35 ms scan baseline to a 0.30 ms median (4.5x). Replaying
the same batch with `DO NOTHING` took 0.21 ms instead of a 35.01 ms scan
baseline—about 164x faster. The harness keeps databases alive until timing ends,
verifies affected-row counts, and checks indexed lookup behavior separately.

## Durable storage benchmark

```sh
cargo run --release --example benchmark_storage
```

This harness opens a fresh persistent directory, ingests typed vector batches,
drops the live catalog without checkpointing, measures WAL recovery, validates
the row count, and measures explicit checkpoint compaction. Every batch is one
atomic WAL record and calls `sync_data` before `insert_rows` returns. Input
generation happens inside the timed loop, so the ingestion number is a
conservative embedded-path measurement rather than raw WAL bandwidth.

The median of three processes on the reference machine on 2026-07-23 was 28.46
ms for ten fsynced 1,000-row × 64-dimension batches (about 351,000 rows/s),
11.56 ms to recover the 2.73 MiB WAL, and 17.46 ms to write a 2.69 MiB
checkpoint. The batching contract matters: one-row transactions would require
10,000 synchronization barriers and are intentionally not represented by this
number.
