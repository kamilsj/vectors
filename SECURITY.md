# Security policy

## Supported code

Security fixes are made on the `main` branch while the project is pre-1.0.
Users should run the latest commit or the newest published release and review
release notes before upgrading persistent data.

## Reporting a vulnerability

Please do not open a public issue for a suspected vulnerability. Use the
repository's [private vulnerability reporting
form](https://github.com/kamilsj/vectors/security/advisories/new) and include:

- the affected commit or version;
- the operating system and deployment mode;
- a minimal reproduction or malformed input, when safe to share;
- the impact you observed; and
- any suggested mitigation.

Reports will be acknowledged through the private advisory. A fix and disclosure
plan depend on severity, exploitability, and compatibility impact.

## Deployment boundaries

`vectors` is pre-1.0. The HTTP server offers an optional bearer token, but does
not provide TLS, roles, tenant isolation, or per-query resource quotas. Keep the
default loopback bind or deploy behind a hardened reverse proxy. Treat SQL
access as trusted database access, use a long random token, protect the data
directory and snapshots with operating-system permissions, and do not expose
the service directly to the public internet. Only one process may own a durable
data directory at a time; do not bypass or delete `vectors.lock` while a server
is running.

WAL records and checkpoints are not encrypted at rest. SQL text, relational
values, and embeddings may be recoverable from those files, so use encrypted
storage when the data requires it and exclude the directory from source control
and public backups.
