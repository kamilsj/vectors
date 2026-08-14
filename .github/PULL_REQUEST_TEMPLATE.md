## Why

Describe the user-visible problem and why this change belongs in `vectors`.

## What changed

- Describe the implementation at a reviewable level.

## Validation

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --all-targets --all-features --locked`
- [ ] Relevant optimized and general query results were compared
- [ ] Performance claims include a reproducible workload
- [ ] Public SQL, API, snapshot, and documentation impacts are described
- [ ] Installer or release changes were dry-run on every affected OS/architecture

## Compatibility

Note any SQL semantics, HTTP API, Rust API, snapshot-format, or supported-install
target impact. Write `None` when there is no compatibility change.

## Release note

Choose exactly one:

- [ ] I added a concise user-visible entry under the appropriate
      `CHANGELOG.md` **Unreleased** heading.
- [ ] No changelog entry is needed. Reason:

Maintainers: apply the `skip-changelog` label only for the second choice, and
apply the most specific release category label available (for example
`enhancement`, `bug`, `performance`, `installer`, or `documentation`).
