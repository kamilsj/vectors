# Changelog

Notable project changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and releases use
semantic versioning while the public API remains pre-1.0.

## Unreleased

### Added

- Specialized `VectorTopK` execution for common exact vector-search queries.
- Parallel scoring with thread-local bounded heaps for large candidate sets.
- Reproducible search and snapshot benchmark example.
- Architecture, performance, security, and roadmap documentation.
- GitHub issue templates, dependency updates, and tagged-release automation.

### Changed

- Snapshot vector I/O now uses reusable contiguous buffers and 1 MiB streams.
- Cosine similarity kernels use an unrolled, compiler-vectorizable loop.

## 0.1.0 - 2026-07-20

### Added

- In-memory SQL engine with relational and fixed-width `VECTOR(n)` values.
- Exact cosine, L2, squared-L2, and dot-product search.
- Scalar hash indexes and hybrid metadata filtering.
- Atomic versioned snapshots, autosave, and catalog revisions.
- Actix HTTP API, interactive shell, and built-in web console.
