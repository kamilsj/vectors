# Changelog

Notable project changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and releases use
semantic versioning while the public API remains pre-1.0.

## Unreleased

### Changed

- Primary-key and unique-constraint checks use maintained internal key maps for
  inserts and idempotent conflict handling.

## 0.2.0 - 2026-07-21

### Added

- Specialized `VectorTopK` execution for common exact vector-search queries.
- Parallel scoring with thread-local bounded heaps for large candidate sets.
- Bounded shared SQL AST caching for repeated queries across cloned handles.
- Typed atomic bulk insertion shared by the Rust API and HTTP ingestion route.
- Reproducible search and snapshot benchmark example.
- Architecture, performance, security, and roadmap documentation.
- GitHub issue templates, dependency updates, and tagged-release automation.
- Checksum-verifying Linux and Windows installers with optional automatic web
  console startup.

### Changed

- Snapshot vector I/O now uses reusable contiguous buffers and 1 MiB streams.
- Cosine similarity kernels use an unrolled, compiler-vectorizable loop.
- Structured JSON ingestion no longer serializes values into SQL and reparses
  them before insertion.
- Append-only insert batches extend scalar hash indexes incrementally instead
  of rebuilding buckets for every existing row.

## 0.1.0 - 2026-07-20

### Added

- In-memory SQL engine with relational and fixed-width `VECTOR(n)` values.
- Exact cosine, L2, squared-L2, and dot-product search.
- Scalar hash indexes and hybrid metadata filtering.
- Atomic versioned snapshots, autosave, and catalog revisions.
- Actix HTTP API, interactive shell, and built-in web console.
