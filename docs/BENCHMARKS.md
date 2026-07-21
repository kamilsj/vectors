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
