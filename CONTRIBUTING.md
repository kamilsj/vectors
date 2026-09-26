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
cargo test --all-targets --all-features
cargo run --bin vectors
cargo run --bin vectors-server
```

The server console is available at `http://127.0.0.1:8080`. For a quick embedded
example, run:

```sh
cargo run --example hybrid_search
```

Console development still needs no frontend build step. To run its optional
browser regression tests, install Node.js 20 or newer, then run:

```sh
npm ci
npx playwright install chromium
npm run test:web
```

The tests start a local static server and mock API responses to exercise slow
connections, authentication changes, invalid queries, and large result sets.
Live API behavior is covered by the Rust integration suite. Set
`PLAYWRIGHT_CHROMIUM_EXECUTABLE` to use an existing Chromium executable locally.

To measure console result rendering, start `node tests/web-server.cjs` in one
terminal and run `npm run benchmark:web` in another. The harness generates its
own deterministic data and reports browser rendering and layout time. Save the
committed renderer with `git show HEAD:web/app.js > /tmp/vectors-web-baseline-app.js`
before changing it, then compare with
`npm run benchmark:web -- --baseline /tmp/vectors-web-baseline-app.js`.
See [the browser benchmark](docs/BENCHMARKS.md#browser-result-rendering) for the
recorded baseline, workload controls, and measurement boundaries.

## Before opening a pull request

Python SDK changes use uv 0.12.10+ and the committed dependency lock:

```sh
uv sync --project python --locked
uv run --project python --locked python python/tools/generate_async.py --check
uv run --project python --locked ruff check python
uv run --project python --locked python -m unittest discover -s python/tests -v
uv build python --out-dir python/dist --clear --no-sources
uv run --project python --locked python python/tools/check_dist.py python/dist --install
```

Set `VECTORS_TEST_SERVER` to a built `vectors-server` executable to include the
temporary live-server tests. Edit the synchronous client and regenerate its
explicit async counterpart with
`uv run --project python --locked python python/tools/generate_async.py`; both
variants are checked in. See [the SDK guide](python/README.md) for platform
instructions and [the release guide](python/PUBLISHING.md) for PyPI publishing.
Python releases use `python-vX.Y.Z` tags independently of Rust releases.

Run the same core checks as CI:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features --locked
cargo doc --all-features --no-deps
cargo package --locked
```

Then check the patch for accidental generated files, local snapshots, or
unrelated formatting changes.

## Release notes

Every pull request must make its release-note intent explicit. User-visible
changes add one concise bullet under the matching `CHANGELOG.md` **Unreleased**
heading: Added, Changed, Deprecated, Removed, Fixed, or Security. Describe the
observable effect rather than the implementation details, and keep related
changes in one bullet when that reads clearly.

Changes with no user-visible effect may use the `skip-changelog` pull-request
label instead. Maintainers apply that label after reviewing the explanation in
the pull-request template. The label only skips the curated changelog entry;
the merged pull request can still appear in GitHub's generated release details.

Use the most specific GitHub label available so generated notes stay grouped:
`breaking-change`, `enhancement`, `bug`, `performance`, `installer`,
`documentation`, `dependencies`, or `maintenance`. Unmatched pull requests are
kept under **Other changes**, so nothing silently disappears.

When preparing a release, move the accumulated entries from **Unreleased** to
`## X.Y.Z - YYYY-MM-DD` and leave an empty categorized **Unreleased** section at
the top. The release workflow requires a matching version section and prepends
it to GitHub's generated release notes before publishing any assets.

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

## Installer and release changes

Keep `install.sh` compatible with POSIX `sh` and `install.ps1` compatible with
Windows PowerShell 5.1. Installer changes must preserve durable data, verify
release checksums before replacing binaries, and leave an existing installation
usable if an upgrade fails. Exercise the network-free target or dry-run mode,
then test the affected native platform before changing its advertised support.
Managed-server changes must preserve the previous bind/storage configuration,
verify process identity, and use cooperative shutdown so final checkpoints and
snapshots complete before replacement.

Updater changes must keep latest-release resolution separate from pinned asset
downloads, verify the installer before executing it, and avoid implicit
downgrades or starting stopped services. Run `python3 tests/test_updater.py` on
POSIX and `powershell -NoProfile -File tests/update_windows.ps1` on Windows.
Use isolated installation/state/data directories for native restart tests;
do not test upgrades against an existing user database.

When adding or renaming a release target, update the installer resolver, release
matrix, checksum generation, smoke tests, and installation documentation in the
same pull request. The archive name is a public contract between the release
workflow and the installers.
Release workflows use full action commit hashes; update the adjacent version
comment when deliberately advancing a pinned action.

Tagged releases test the console in a browser and run the compiled server's
embedded interface, SQL/vector APIs, chunking, GraphRAG, and durable restart
checks before publication. The same smoke test runs against installed release
downloads on every supported platform. Run it locally with an isolated temporary
database (created and removed by the script):

```sh
python3 scripts/release_smoke.py --server target/release/vectors-server --expected-version v0.9.0
```

## Performance changes

Include a reproducible workload rather than a single elapsed-time claim. Report
the build profile, CPU, operating system, row count, dimensions, filter
selectivity, distance metric, and `LIMIT`. Confirm that the old and new paths
return equivalent results. GPU results must also identify the adapter, driver,
compute policy, cache state, and whether initialization or upload is included.
See [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

Avoid adding architecture-specific or unsafe code without prior design
discussion. The crate currently forbids `unsafe` code. CPU kernels use safe
loops that LLVM can vectorize; the optional accelerator path uses WGSL through
wgpu and must retain a tested portable fallback.

## Pull requests

Keep commits scoped to one concern and write messages in terms of observable
behavior. A pull request should explain why the change is needed, how it was
validated, and whether it affects SQL, API, or snapshot compatibility. Update
the README, changelog, or architecture notes when the public behavior changes.

By participating, be respectful, assume good intent, and keep review focused on
the work. Harassment, personal attacks, and discriminatory conduct are not
accepted in project spaces.
