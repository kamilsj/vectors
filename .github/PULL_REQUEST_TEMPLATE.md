## Why

Describe the user-visible problem and why this change belongs in `vectors`.

## What changed

- Describe the implementation at a reviewable level.

## Validation

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test --all-targets --locked`
- [ ] Relevant optimized and general query results were compared
- [ ] Performance claims include a reproducible workload
- [ ] Public SQL, API, snapshot, and documentation impacts are described

## Compatibility

Note any SQL semantics, HTTP API, Rust API, or snapshot-format impact. Write
`None` when there is no compatibility change.
