# Contributing

Bug reports, workload descriptions, documentation fixes, and focused pull
requests are welcome. The most useful contributions start with observable
behavior: a query, data shape, expected result, and what happened instead.

For a large feature or architectural change, open an issue before investing in
an implementation. SQL semantics, persistence compatibility, and planner
behavior are easier to settle while a design is still small.

## Development setup

Install the stable Rust toolchain and clone the repository. No external service,
code generator, or frontend toolchain is required.

```sh
cargo test --all-targets
cargo run --bin vectors
cargo run --bin vectors-server
```

The server console is available at `http://127.0.0.1:8080`. For a quick embedded
example, run:

```sh
cargo run --example hybrid_search
```

## Before opening a pull request

Run the same core checks as CI:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets --locked
cargo doc --no-deps
cargo package --locked
```

Then check the patch for accidental generated files, local snapshots, or
unrelated formatting changes.

## Correctness expectations

- SQL changes need tests for successful behavior and relevant error cases.
- Planner optimizations must be compared with the general executor on the same
  input. An optimization may change cost, never query meaning.
- Vector math changes need empty, invalid, unequal-dimension, and numerical
  edge-case coverage where applicable.
- Failed multi-statement writes must remain atomic.
- Snapshot readers must validate lengths before allocating or indexing.
- Snapshot format changes must stay backward compatible or include a documented
  version and migration policy.

The deeper design constraints are recorded in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Performance changes

Include a reproducible workload rather than a single elapsed-time claim. Report
the build profile, CPU, operating system, row count, dimensions, filter
selectivity, distance metric, and `LIMIT`. Confirm that the old and new paths
return equivalent results. See [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

Avoid adding architecture-specific or unsafe code without prior design
discussion. The crate currently forbids `unsafe` code and relies on safe loops
that LLVM can vectorize.

## Pull requests

Keep commits scoped to one concern and write messages in terms of observable
behavior. A pull request should explain why the change is needed, how it was
validated, and whether it affects SQL, API, or snapshot compatibility. Update
the README, changelog, or architecture notes when the public behavior changes.

By participating, be respectful, assume good intent, and keep review focused on
the work. Harassment, personal attacks, and discriminatory conduct are not
accepted in project spaces.
