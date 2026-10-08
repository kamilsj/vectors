# Saywit compatibility

Saywit's current semantic adapter stores turn and attachment units in a regular
SQL table, `saywit_units_v1`, with indexed `chat_id` and `turn_id` columns. Its
query uses bound values for the embedding profile, permitted chat IDs, and an
optional excluded turn, then ranks by exact cosine distance. This contract was
inspected in the Saywit checkout on 2026-10-09.

The updated planner uses the existing chat index for `chat_id IN (...)`. It
deduplicates membership values and preserves row order for stable vector ties.
An `AND` term proven by the index is omitted from subsequent row evaluation;
the profile and excluded-turn predicates still run before top-k. Partially
indexed `OR` expressions retain their complete predicate. NULL handling,
`NOT IN`, row-dependent lists and incompatible types retain SQL semantics.
No Saywit query, schema migration, reindex, or embedding change is required.

## Verify the actual adapter

Build the updated server, then use Saywit's Python environment:

```sh
cargo build --release --locked --bin vectors-server
/path/to/saywit/.venv/bin/python scripts/check_saywit_compat.py \
  --saywit /path/to/saywit \
  --server target/release/vectors-server
```

The check imports only `semantic/transport.py`, configures isolated Django
settings, and starts a temporary authenticated server with durable storage. It
uses synthetic vectors with the adapter's declared dimensions, exercises the
real HTTP transport, checks normalized bulk insertion, scope/profile/turn
filters, quoted and repeated chat IDs, a 500-chat scope, exact scores, restart,
and deletion. It makes no embedding-provider calls and reads no user messages
or application environment files. Both the server and test data are removed
on exit. The output identifies the adapter file tested.

The query benchmark is separate:

```sh
cargo run --release --locked --example benchmark_scoped_search -- 20000 1024 7
```

Arguments are SQL rows, vector dimensions, and measured queries per scope.
Each run compares every result and score with an independent full-scan and
full-sort SQL reference. See [measurements](BENCHMARKS.md#saywit-scoped-message-search).
These are regular SQL rows; the GraphRAG collection limit remains 10,000 chunks.

## Accuracy and access boundaries

Saywit remains responsible for current chat membership, Smart/private visibility,
and rechecking that indexed source versions still exist and are accessible.
Vectors applies the submitted scope before ranking, but a filter itself does
not authenticate a user's entitlement to a chat. The server stays behind the
application with its existing bearer-token authentication.

The compatibility check establishes transport, persistence, filtering and exact
ranking equivalence. Semantic recall requires a labeled set of representative
Saywit queries and their expected messages; this test does not evaluate a live
embedding provider, attachment extraction, or the full Django search endpoint.
