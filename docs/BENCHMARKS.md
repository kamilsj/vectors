# Benchmarks

Performance work in `vectors` starts with a reproducible query and a correctness
check. The repository benchmarks compare execution, parsing, and browser
rendering paths inside this project; they are not presented as comparisons
with another database.

## Run the benchmark

```sh
cargo run --release --example benchmark_vector_search
cargo run --release --features gpu --example benchmark_vector_search -- --compute auto
cargo run --release --features gpu --example benchmark_vector_search -- --compute gpu
```

The first command deliberately defaults to `cpu`, which keeps historical CPU
regression runs comparable. `--compute auto` lets the engine choose a GPU above
its crossover and fall back safely; `--compute gpu` requires GPU execution and
fails if the scan, cache, adapter, or build is incompatible. The same policy can
be supplied through `VECTORS_COMPUTE_DEVICE` when `--compute` is omitted.

The benchmark:

1. creates a table with relational metadata and fixed-width vectors;
2. inserts deterministic data through SQL;
3. builds a scalar hash index for the filter;
4. verifies that `VectorTopK` and the general executor return the same neighbor
   primary keys, independent of tie ordering and floating-point formatting;
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
| `VECTORS_COMPUTE_DEVICE` | `cpu` | — | Search policy when `--compute` is omitted: `cpu`, `auto`, or `gpu` |
| `VECTORS_GPU_MIN_ELEMENTS` | `8388608` | — | Candidate count × dimensions before `auto` tries a GPU |
| `VECTORS_GPU_CACHE_BYTES` | `536870912` | — | Bound for cached dense GPU columns |

PowerShell example:

```powershell
$env:VECTORS_BENCH_ROWS = "100000"
$env:VECTORS_BENCH_DIMENSIONS = "384"
$env:VECTORS_BENCH_ITERATIONS = "20"
cargo run --release --example benchmark_vector_search
```

GPU example with a larger exact scan:

```powershell
$env:VECTORS_BENCH_ROWS = "1000000"
$env:VECTORS_BENCH_DIMENSIONS = "384"
$env:VECTORS_BENCH_ITERATIONS = "20"
cargo run --release --features gpu --example benchmark_vector_search -- --compute gpu
```

The correctness query runs before timing, so GPU initialization and the first
dense-column upload are warm in the reported query averages. That is a
steady-state scan measurement, not cold-start latency. Initialization, upload,
host readback, scalar-index filtering, and CPU top-k selection remain real costs
at their respective boundaries; publish separate cold and warm numbers if both
matter to the workload.

## CPU and GPU comparisons

The optional wgpu backend performs exhaustive scoring. It is not an ANN index
and does not change recall intentionally. GPU arithmetic uses `f32`, while the
CPU path may accumulate intermediate values with different precision. The
harness therefore compares the sorted primary-key sets returned by the
optimized and general executors rather than requiring byte-identical distance
values or an arbitrary order among SQL ties.

For a useful device comparison, run separate processes with `--compute cpu`
and `--compute gpu` using identical rows, dimensions, filter selectivity,
metric, iteration count, release profile, and locked dependency graph. Report
the adapter and driver alongside the CPU, operating system, and Rust version.
Also state whether the column was warm in the GPU cache. `auto` is appropriate
for deployment experiments, but it is not proof that a particular query ran on
an accelerator because fallback is part of that policy.

No GPU latency or throughput number is published here yet. A result belongs in
this document only after repeat runs on named hardware establish a crossover
and the neighbor-ID correctness check passes.

## CPU scan scheduling

The scan-layout harness isolates exact CPU search from SQL ingestion and
snapshot I/O. It generates deterministic vectors through typed batches and
checks every result, including scores, against the general SQL executor before
timing. It tests cosine distance, squared L2, and dot product, both unfiltered
and with a hash-indexed 50% filter. Every query returns 20 rows after an offset
of 3. General-executor ties are explicitly ordered by source ID.

```sh
cargo build --release --locked --example benchmark_scan_layout
RAYON_NUM_THREADS=16 VECTORS_BENCH_ROWS=32768 \
  VECTORS_BENCH_DIMENSIONS=384 VECTORS_BENCH_BATCH_ROWS=32768 \
  VECTORS_BENCH_ITERATIONS=80 target/release/examples/benchmark_scan_layout
```

`VECTORS_BENCH_BATCH_ROWS` defaults to the entire row count; change it to `500`
to exercise fragmented append slabs. Other defaults are 32,768 rows, 384
dimensions, and 60 timed iterations. Change `RAYON_NUM_THREADS` in separate
processes to measure scaling. The output includes per-process p50, p95, and
mean latency after five warm-up queries; initialization and ingestion are
excluded. This harness forces CPU execution, even in a GPU-enabled build.

On 2026-09-17, balanced scan ranges and dimension-aware task granularity were
compared with the scan engine at `9238cc8` on an Apple M4 Max (16 CPU cores,
48 GiB memory), macOS 27.0 (26A428), Rust 1.97.1, default release profile, no
custom target-CPU flags, and locked dependencies. Each configuration ran in
three processes with 80 timed queries per metric/filter, alternating baseline
and updated binaries. The following cosine latencies are the median of the
three process p50 values; all metrics and process results are retained in the
[raw CSV](benchmarks/cpu-scan-layout-2026-09-17.csv).

| Rows × dimensions | Ingest batch | Threads | Filter | Before | After | Speedup |
| --- | ---: | ---: | --- | ---: | ---: | ---: |
| 32,768 × 64 | 32,768 | 16 | none | 0.628 ms | 0.212 ms | 2.97× |
| 32,768 × 64 | 32,768 | 16 | 50% | 0.493 ms | 0.233 ms | 2.12× |
| 32,768 × 384 | 32,768 | 16 | none | 0.618 ms | 0.493 ms | 1.25× |
| 32,768 × 384 | 32,768 | 16 | 50% | 0.651 ms | 0.440 ms | 1.48× |
| 8,192 × 1,536 | 8,192 | 16 | none | 0.719 ms | 0.616 ms | 1.17× |
| 8,192 × 1,536 | 8,192 | 16 | 50% | 0.650 ms | 0.494 ms | 1.32× |
| 32,768 × 384 | 500 | 16 | none | 0.468 ms | 0.480 ms | 0.97× |
| 32,768 × 384 | 500 | 16 | 50% | 0.649 ms | 0.439 ms | 1.48× |
| 32,768 × 384 | 32,768 | 1 | none | 2.931 ms | 2.841 ms | 1.03× |
| 32,768 × 384 | 32,768 | 1 | 50% | 2.515 ms | 2.346 ms | 1.07× |

The largest gain removes repeated slab lookups and excessive small tasks for
a single-slab scan. Larger slabs can also use more cores instead of assigning
one worker per slab. Indexed scans benefit from fewer small heaps and merges.
Already-fragmented full scans were mixed: cosine and dot product regressed by
2.6% and 3.9%, while squared L2 improved by 5.8%. These local measurements do
not establish a universal improvement, GPU crossover, or cross-database ranking.
Repeat the harness on the intended hardware and workload before tuning threads.

## SQL column lookup

SQL projections and residual filters resolve column names repeatedly while
scanning rows. The executor now compares names without allocating lowercase
copies for each lookup. This preserves its ASCII-insensitive identifier rules,
qualified-name resolution, and missing-column errors.

The [benchmark harness](../examples/benchmark_column_lookup.rs) exercises
scalar top-k and exact cosine search with an unindexed modulo filter. It checks
both against general-executor results before timing. The scalar query orders by
the last metadata column; the vector query has a single vector sort key. Both
use `LIMIT 20 OFFSET 3`; approximately half the rows qualify. Timings use the
normal SQL entry point with a warm parse cache, include execution and result
construction, exclude data generation and insertion, and follow five warm-up
queries.

```sh
cargo build --release --locked --example benchmark_column_lookup
RAYON_NUM_THREADS=16 VECTORS_BENCH_ROWS=16384 VECTORS_BENCH_COLUMNS=32 \
  VECTORS_BENCH_DIMENSIONS=64 VECTORS_BENCH_ITERATIONS=40 \
  target/release/examples/benchmark_column_lookup
```

Local measurements on 2026-09-18 used an Apple M4 Max (16 CPU cores, 48 GiB),
macOS 27.0 (26A428), Rust 1.97.1, and the release profile without custom CPU
flags. Each cell below is the median of the per-process p50 from three
alternating before/after pairs; each process ran 40 timed queries. All 24
processes passed result-equivalence checks. Data had 16,384 rows, 64-dimensional
vectors, and either 4 or 32 additional integer metadata columns.

| Metadata columns | Threads | Query | Before p50 | After p50 | Ratio |
| --- | --- | --- | --- | --- | --- |
| 4 | 1 | Scalar top-k | 5.949 ms | 2.123 ms | 2.80× |
| 4 | 1 | Filtered cosine | 3.354 ms | 1.009 ms | 3.33× |
| 4 | 16 | Scalar top-k | 6.121 ms | 2.022 ms | 3.03× |
| 4 | 16 | Filtered cosine | 0.811 ms | 0.337 ms | 2.41× |
| 32 | 1 | Scalar top-k | 20.170 ms | 3.786 ms | 5.33× |
| 32 | 1 | Filtered cosine | 13.051 ms | 2.255 ms | 5.79× |
| 32 | 16 | Scalar top-k | 21.755 ms | 3.323 ms | 6.55× |
| 32 | 16 | Filtered cosine | 3.494 ms | 0.398 ms | 8.79× |

The [raw CSV](benchmarks/sql-column-lookup-2026-09-18.csv) includes all 48 query
measurements and qualifying-row counts. The baseline used the same worktree
before the column-lookup change (engine SHA-256
`7bf248f9d9056eaadf1153059d2961c03119b7a8be44854da70080ea073cf6b9`).
Filtered-vector baseline latency varied noticeably between processes; use the
raw values and repeat on your workload. These results measure name-resolution
overhead in this workload, not a comparison with other databases or an
across-the-board vector-kernel speedup.

## Browser result rendering

The console harness measures `renderDataTable` DOM construction, insertion,
style calculation, and forced layout in headless Chromium. It generates rows
before timing and mocks the health and table-list APIs. Network transfer,
response JSON parsing, SQL execution, and vector scoring are excluded; a Rust
server is not needed. The full result remains available in browser memory,
while the updated renderer displays one page at a time and expands large cell
values only when requested.

Install the optional browser tools, save a baseline before editing the
renderer, and start the static asset server:

```sh
npm ci
npx playwright install chromium
git show HEAD:web/app.js > /tmp/vectors-web-baseline-app.js
node tests/web-server.cjs
```

After changing the renderer, run the comparison from another terminal:

```sh
npm run --silent benchmark:web -- \
  --baseline /tmp/vectors-web-baseline-app.js \
  --rows 10000 --dimensions 384 --seed 42 --warmups 1 --repetitions 5 \
  > /tmp/vectors-web-render.json
```

Omit `--baseline` to measure the current renderer alone; use `--format csv` for
a compact CSV summary. `VECTORS_WEB_URL` changes the static server URL from
`http://127.0.0.1:4173`, and `PLAYWRIGHT_CHROMIUM_EXECUTABLE` selects an existing
Chromium executable. Both variants use the current HTML and CSS; only the
baseline JavaScript asset is replaced. Before timing, the harness checks the
first 100 scalar and vector previews against the generated data, verifies the
complete first vector, and checks that collapsing an expanded vector removes
its full text from the DOM.

The recorded run on 2026-09-17 used Apple M4 Max, macOS 27.0 (26A428), Chrome
152.0.7977.83, and a 1440 × 1000 viewport. The dataset contained 10,000 rows with
an integer ID, a short title, and a 384-dimensional vector, generated with seed
42. Each variant ran in its own page context in one browser process, with one
warmup after verification and five measured renders. The baseline was
`web/app.js` from commit `4a2a3f4e2c3da838ce031c43bdf2bbbaf479c201`, which was
`HEAD` before these changes. To reproduce this specific baseline after later
commits, replace the `git show HEAD:web/app.js` command above with:

```sh
git show 4a2a3f4e2c3da838ce031c43bdf2bbbaf479c201:web/app.js > /tmp/vectors-web-baseline-app.js
```

| Local rendering metric | Baseline | Updated |
| --- | ---: | ---: |
| Median DOM construction + layout | 204.1 ms | 2.8 ms |
| Rows initially rendered | 10,000 | 100 |
| DOM nodes inside the result container | 70,017 | 1,139 |
| DOM elements inside the result container | 40,011 | 724 |

The 72.9× initial-render speedup comes from rendering the default 100-row page
instead of all 10,000 rows and deferring complete vector serialization until a
cell is expanded. All 10,000 rows remain accessible through pagination. This
single-machine result measures reduced browser work; it is not an API latency,
database throughput, or universal performance claim. The
[raw JSON](benchmarks/web-render-2026-09-17.json) preserves every measured sample
and verification result. Repeat the harness on the intended browser, hardware,
and result shape before drawing broader conclusions.

## Reference result

This result is the median of three local benchmark processes recorded on
2026-07-21, with 20 timed iterations per process. It should not be used to claim
a ranking against other databases. It predates the dense-column and optional
GPU work and remains a historical regression reference, not a current-device
claim.

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

## Vector API and response encoding

The [HTTP harness](../scripts/benchmark_api.py) compares two local server
binaries with synthetic in-memory data and a reused HTTP connection. The
baseline is the v0.7.0 working tree immediately before the typed-search,
owned-ingestion, and direct-response changes, including preceding unreleased
improvements. Both binaries use the same SQL scoring kernels. Preserve a
baseline binary before applying changes, then run:

```sh
cargo build --release --locked --bin vectors-server
cp target/release/vectors-server /tmp/vectors-server-before
# Apply the API changes and rebuild.
cargo build --release --locked --bin vectors-server
python3 scripts/benchmark_api.py \
  --baseline /tmp/vectors-server-before \
  --candidate target/release/vectors-server \
  --output /tmp/vector-api.json
```

Recorded on 2026-09-19: Apple M4 Max, 16 physical cores, 48 GiB RAM, macOS
27.0, Rust 1.97.1, release profile, CPU compute, four Rayon threads, two HTTP
workers, two blocking threads per worker, four admitted database tasks, and
one client. Each case has five warmups and 30 measured requests in each of
three paired runs; server order alternates between pairs. No other project
builds or tests ran during the recorded measurements.

| HTTP workload | Before p50 | After p50 | Speedup |
| --- | ---: | ---: | ---: |
| Search, 32 rows × 1,536 dimensions | 1.363 ms | 0.160 ms | 8.51× |
| Indexed search, 4,096 × 384, 64 candidates | 0.459 ms | 0.128 ms | 3.59× |
| Full scan, 4,096 × 384 | 0.569 ms | 0.235 ms | 2.42× |
| SQL response, 1,000 vectors × 384 | 9.221 ms | 7.598 ms | 1.21× |
| Import, 256 rows × 64, 32 text fields, normalization | 3.068 ms | 1.886 ms | 1.63× |

Numbers are medians of the three per-run p50s. Search uses exact cosine
ranking, `LIMIT 10`, varying query vectors, and selects only `id` plus the
computed score. The indexed case uses equality on one of 64 category values.
Ingestion targets a freshly created table each time, with table creation/drop
outside the timer. These import measurements exclude durable WAL I/O.

The timer includes HTTP, server JSON decoding, validation, execution, result
encoding, and response transfer. Client request encoding and response parsing
are outside it. Complete response bytes matched the baseline in every measured
pair. The [raw JSON](benchmarks/vector-api-2026-09-19.json) contains individual
samples, p95s, response sizes/hashes, binary hashes, and workload parameters.
This is a local comparison within this project; it does not measure competing
databases, GPU throughput, saturated concurrency, or ANN recall.

The largest gains occur when formatting/parsing a high-dimensional query
previously dominated search time. Large scans remain bounded by scoring work.
Response encoding now traverses typed values directly on admitted workers,
avoiding the intermediate JSON-value tree; imports reuse column lookup state
and owned buffers. For an engine-only comparison that also includes residual
filters, run `cargo run --release --locked --example benchmark_typed_search`.
That harness compares the old SQL-conversion route against typed search,
alternates order, varies query vectors, and checks every result for equality.

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

## Hybrid RAG retrieval and lexical-cache reuse

```sh
cargo run --release --example benchmark_rag_retrieval -- 64 16 128 20
```

Recorded on 2026-09-19 using an Apple M4 Max (16 logical CPUs, 48 GiB RAM),
macOS 27.0 arm64, Rust 1.97.1, release profile, and CPU compute. The deterministic
fixture contains 64 documents, 16 chunks each (1,024 total), and 128-dimensional
vectors. Retrieval combines BM25/vector ranks, one graph hop, and MMR selection:
40 candidates, 12 graph seeds, 8 neighbors, 10 results, diversity 0.3, up to
3 results per document, and a 24,000-byte context budget.

| Lexical index state | Median | p95 |
| --- | ---: | ---: |
| Rebuilt after a catalog revision | 7.294 ms | 7.598 ms |
| Reused for the same revision | 0.380 ms | 0.416 ms |

Across 20 cold/warm pairs, warm local retrieval was **19.2× faster at
the median**. Every pair asserted identical ranked hits, scores, edges, and
context bytes. Queries vary across the fixture; there is no response-result
cache. In this original measurement, a separate marker-table write invalidated
the catalog revision before each cold run; its write cost was excluded.
The cache now survives unrelated writes. The current harness instead performs
a no-op update to chunk text to force a rebuild, also outside the timer. The
numbers above describe the original implementation, not the updated harness.

This measures the benefit of reusing lexical tokenization/postings within the
new RAG pipeline, not an end-to-end speedup over another database or embedding
provider. It excludes ingestion, query embedding, external reranking, HTTP, and
network latency. The synthetic fixture does not establish real-world retrieval
quality. Workload parameters are positional: documents, chunks per document,
vector dimensions, and measured pairs. The
[raw results](benchmarks/rag-retrieval-2026-09-19.json) include source hashes and
environment details.

## RAG retrieval while other data changes

The lexical index now follows the chunk table's storage generation instead of
the database-wide revision. Relationship upserts and unrelated table writes
therefore preserve cached tokenization and posting lists. Chunk writes still
invalidate the index, including text-only SQL updates.

The [cache-churn harness](../examples/benchmark_rag_churn.rs) compares the working
tree immediately before these changes with the updated implementation:

```sh
cargo build --release --locked --example benchmark_rag_churn
target/release/examples/benchmark_rag_churn 64 16 128 20 /tmp/rag-results.json
```

Preserve a baseline executable before applying the changes, then run both
binaries with the same arguments and separate result paths. The optional file
contains complete canonical results for byte comparison. Each query measures
four phases: after a no-op chunk text update, an immediate repeat, after an
unrelated scalar update, and after an idempotent relationship upsert. The
relationship upsert deliberately preserves endpoints, kind, and weight so
results remain comparable across all phases; actual relationship content edits
are covered by correctness tests.

Measurements on 2026-09-19 used an Apple M4 Max (16 CPU cores, 48 GiB), macOS
27.0 (26A428), Rust 1.97.1, locked dependencies, and CPU release builds. Three
paired process runs alternated baseline/current order, with 20 queries per
phase per process. These are medians across the resulting 60 samples per phase:

| Retrieval phase | Before median | After median | Before p95 | After p95 |
| --- | ---: | ---: | ---: | ---: |
| After chunk text update (cold) | 7.364 ms | 7.422 ms | 7.808 ms | 8.185 ms |
| Immediate repeat (warm) | 0.355 ms | 0.352 ms | 0.378 ms | 0.399 ms |
| After unrelated table update | 7.321 ms | 0.348 ms | 7.676 ms | 0.394 ms |
| After idempotent relationship upsert | 7.397 ms | 0.367 ms | 7.724 ms | 0.412 ms |

The two non-chunk write phases improved by **21.1×** and **20.2×** at the median:
cache hits increased from 0/60 to 60/60 in both. Cold and already-warm retrieval
were essentially unchanged. Every run and phase returned byte-identical
canonical hits, scores, citations, edges, candidate counts, and context sizes;
revision and cache-hit telemetry were excluded from that comparison.

The fixture has 1,024 chunks with 128-dimensional vectors, 40 candidates,
10 selected results, and the same MMR/context limits as the earlier harness.
Graph expansion is disabled (`max_hops=0`) to isolate cache behavior and preserve
ranking across versions. These timings do **not** measure the new graph beam's
quality or speed. They exclude writes, ingestion, provider calls, HTTP, result
serialization, and persistence. No other project builds or tests ran during
timing, but desktop background activity was not controlled. This is one
synthetic local workload, not a general throughput or cross-database claim.
The [raw report](benchmarks/graph-retrieval-2026-09-19.json) retains all samples,
workload settings, result fingerprints, and source/binary hashes.
