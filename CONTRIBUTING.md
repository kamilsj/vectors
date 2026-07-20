# Contributing

Bug reports and focused pull requests are welcome. For larger changes, open an
issue first so the SQL behavior and storage implications can be discussed
before implementation work begins.

## Development setup

The project uses the stable Rust toolchain. Before submitting a change, run:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Changes to SQL execution or storage should include regression tests. Snapshot
format changes must either remain backward compatible or increment the format
version with a documented migration path.

Keep commits scoped to one concern and describe observable behavior in the
commit message. Avoid unrelated formatting changes in functional patches.
