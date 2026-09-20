# Publishing vectors-sdk

The distribution name is `vectors-sdk`; applications import `vectors_sdk`.
The version in `pyproject.toml` is the single source for package metadata,
`vectors_sdk.__version__`, and the HTTP User-Agent. The initial 0.1.0 release
is prepared locally and has not been uploaded to PyPI.

## One-time PyPI setup

Sign in to the PyPI account that should own the package and open
[Publishing](https://pypi.org/manage/account/publishing/). Register a pending
GitHub trusted publisher using these exact values:

| Field | Value |
| --- | --- |
| PyPI project name | `vectors-sdk` |
| GitHub owner | `kamilsj` |
| Repository | `vectors` |
| Workflow filename | `python-publish.yml` |
| Environment name | `pypi` |

Create the matching `pypi` environment in the GitHub repository settings.
Limit its deployment tags to `python-v*`; add required reviewers if the
repository's release policy calls for them. The first successful workflow
upload creates the PyPI project. For an existing project, add the publisher
under that project's Publishing settings instead. See
[PyPI's pending-publisher guide](https://docs.pypi.org/trusted-publishers/creating-a-project-through-oidc/).

The dedicated workflow builds and tests in a job without publishing access.
A separate job downloads only the verified distributions and exchanges its
GitHub identity for short-lived PyPI credentials. It has `id-token: write`
and uses `uv publish --trusted-publishing always`; no stored PyPI token is
required. See [uv's GitHub publishing guide](https://docs.astral.sh/uv/guides/integration/github/).

## Build and verify

Run from the repository root using uv 0.12.10+:

```sh
uv sync --project python --locked
uv run --project python --locked python python/tools/generate_async.py --check
uv run --project python --locked ruff check python
cargo build --locked --bin vectors-server
VECTORS_TEST_SERVER=target/debug/vectors-server uv run --project python --locked python -m unittest discover -s python/tests -v
uv build python --out-dir python/dist --clear --no-sources
VECTORS_TEST_SERVER=target/debug/vectors-server uv run --project python --locked python python/tools/check_dist.py python/dist --install
```

For PowerShell, set `$env:VECTORS_TEST_SERVER = "target/debug/vectors-server.exe"`
before running the uv commands, omitting the inline environment prefix.

The distribution check verifies metadata, the typing marker, source-package
contents, strict PyPI README rendering, and SDK tests in isolated environments
installed from both artifacts. `--clear` removes stale artifacts before a build.
The source archive includes tests, generation tools, and `uv.lock`. Only HTTPX
is a direct runtime dependency; development tools are excluded from the wheel.

## Release through GitHub

For the first release, keep version `0.1.0`. For subsequent releases, update
the version and lockfile together:

```sh
uv version --project python --bump patch
uv lock --project python
```

Update release notes, run the checks above, commit the SDK, lockfile, workflow,
and required server changes, and push the reviewed revision. Tag that revision
with the exact SDK version, for example:

```sh
git tag python-v0.1.0
git push origin python-v0.1.0
```

The workflow checks that the tag and package version match before building.
After it succeeds, verify installation in a fresh application:

```sh
uv init --bare /tmp/vectors-sdk-release-check
cd /tmp/vectors-sdk-release-check
uv add --index-url https://pypi.org/simple vectors-sdk==0.1.0
uv run python -c "import vectors_sdk; print(vectors_sdk.__version__)"
```

Then remove the initial unpublished notice in the documentation and use
`uv add vectors-sdk` or `python -m pip install vectors-sdk` for normal installs.
PyPI releases are immutable; corrections need a new version. The workflow uses
`--check-url` so retries can skip artifacts that have already been uploaded.

## Manual upload or TestPyPI

The project declares explicit publishing URLs for `pypi` and `testpypi`.
To upload verified artifacts locally, provide a PyPI API token through the
`UV_PUBLISH_TOKEN` environment variable using your secret manager, then run:

```sh
uv publish --project python --index pypi --trusted-publishing never python/dist/*
```

For TestPyPI, use `--index testpypi` and a token issued by TestPyPI instead.
Its explicit index is excluded from ordinary dependency resolution. Never
commit credentials. See [uv's packaging guide](https://docs.astral.sh/uv/guides/package/)
for authentication and publishing options.
