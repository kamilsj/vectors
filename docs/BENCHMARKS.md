# Benchmarks

Performance work in `vectors` starts with a reproducible query and a correctness
check. The repository benchmark compares two execution paths inside this
project; it is not presented as a comparison with another database.

## Run the benchmark

```sh
cargo run --release --example benchmark_vector_search
```

The benchmark:

1. creates a table with relational metadata and fixed-width vectors;
2. inserts deterministic data through SQL;
3. builds a scalar hash index for the filter;
4. verifies that `VectorTopK` and the general executor return the same rows;
5. times both paths; and
6. saves and reloads a snapshot.

The generated snapshot is removed after the run. No network service is involved.

## Workload controls

Environment variables make the data shape repeatable:

| Variable | Default | Meaning |
| --- | ---: | --- |
| `VECTORS_BENCH_ROWS` | `20000` | Number of generated rows |
| `VECTORS_BENCH_DIMENSIONS` | `64` | Dimensions per vector |
| `VECTORS_BENCH_ITERATIONS` | `8` | Timed query repetitions |

PowerShell example:

```powershell
$env:VECTORS_BENCH_ROWS = "100000"
$env:VECTORS_BENCH_DIMENSIONS = "384"
$env:VECTORS_BENCH_ITERATIONS = "20"
cargo run --release --example benchmark_vector_search
```

## Reference result

This result is a local regression baseline recorded on 2026-07-21. It should
not be used to claim a ranking against other databases.

| Item | Value |
| --- | --- |
| CPU | Intel Core i9-14900KS |
| Memory | 128 GiB |
| OS/toolchain | Windows x86-64 MSVC, Rust 1.96.1 |
| Dataset | 10,000 rows, 64 dimensions, 50% scalar-filter selectivity |
| Query | cosine distance, exact top 20 |
| Optimized SQL | 0.71 ms average |
| General SQL | 11.76 ms average |
| In-engine speedup | 16.5x |
| Snapshot | 2.95 MiB; 35.3 ms save; 7.7 ms load |

The optimized query exercises hash-index pruning, one-time query-vector
evaluation, direct distance scoring, bounded heaps, and deferred projection.
The comparison query adds an arithmetic projection to select the general SQL
executor while keeping the result set equivalent.

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
