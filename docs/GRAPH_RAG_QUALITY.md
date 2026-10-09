# GraphRAG retrieval quality

Research reviewed on 2026-10-09. The immediate change makes graph expansion
honor the same keyword/vector priorities as direct hybrid retrieval and removes
an artificial relevance bonus for zero-fit bridge passages. It does not add
provider calls, change configuration defaults, require reindexing, or implement the
research systems below in full.

Graph-context scores and order can change with any weight policy. Direct rank
fusion is unchanged. Structural path strength still permits weak bridges to
reach relevant evidence; bridge relevance no longer receives a minimum bonus.

## Findings and implementation choices

| Primary source | Relevant finding | Application to Vectors |
| --- | --- | --- |
| [Anthropic: Contextual Retrieval](https://www.anthropic.com/engineering/contextual-retrieval) | Keyword retrieval covers exact identifiers that semantic retrieval can miss; chunk context and reranking complement hybrid retrieval. | Preserve both channels and their configured priorities throughout graph expansion. Vectors already ranks contextual `embedding_text` with BM25 and supports optional external reranking. |
| [PathRAG](https://arxiv.org/abs/2502.14902) | Pruning redundant graph information and retaining relational paths can improve the context supplied to generation. | Keep bounded, query-aware admission and snapshot-owned `retrieval_path` provenance. Correct channel weighting before adding more graph candidates. |
| [HippoRAG 2](https://arxiv.org/abs/2502.14802) | Personalized PageRank, passage integration and an LLM-based retrieval step support associative retrieval in the authors' system. | Evaluate bounded diffusion as a future alternative on multi-hop questions. Do not assume that graph connectivity or multiple paths constitute independent factual corroboration. |
| [Microsoft GraphRAG local search](https://microsoft.github.io/graphrag/query/local_search/) and [DRIFT](https://microsoft.github.io/graphrag/query/drift_search/) | Local search combines graph and source passages; DRIFT uses community information and follow-up searches. | Evaluate question routing and expansion for cross-document questions separately from latency-sensitive factual chat retrieval. Community reports and DRIFT are not currently implemented here. |

These are design directions, not transferable accuracy estimates. Published
results use different corpora, graph structures, models and evaluation methods.
The weighting change is a local consistency fix motivated by inspection of
Vectors; none of the papers prescribes this exact scoring formula.

## Reproduce the targeted quality check

```sh
cargo test --locked --test graph_rag_weights
cargo run --locked --release --example benchmark_graph_quality -- 20
```

The benchmark contains 90 controlled candidate-admission probes: five weight
policies, two deliberately competing semantic similarities, one to three hops,
and outgoing/incoming/bidirectional traversal. Both answer candidates sit outside
the initial direct candidate pool. A one-neighbor budget forces the engine to
choose which evidence to retain. Three-hop probes reserve space for two bridges
and the answer. Synthetic embeddings make the conflict reproducible without
embedding providers or private documents.

The regression also checks final context selection, retained path depth, weight
rescaling, an excluded bridge, and useful third-hop evidence behind two zero-fit
bridges with only one context slot. Existing GraphRAG suites cover document and
relationship filters, seed diversity, cycles, reranking, and snapshot consistency.
The benchmark reports warm retrieval latency without ingestion, providers, or
generation. Its pass count measures these designed cases, not real-world search
accuracy. See the recorded [before/after result](benchmarks/graph-weight-quality-2026-10-09.json).

| Controlled result | v0.11.3 baseline | Updated scoring |
| --- | ---: | ---: |
| Intended evidence retained | 72 / 90 | 90 / 90 |
| Median warm search, median of five rounds | 7.88 µs | 7.79 µs |
| p95 warm search, median of five rounds | 15.38 µs | 15.13 µs |

Each round runs 100 repetitions per case, alternating the two separately
compiled executables. The small timing differences are not evidence of a speed
improvement. Both directions of the original weighting failure account for nine
cases each. The 55 focused engine tests and 62 retrieval/chat API tests pass;
the latter use local mock providers.

## Next quality experiments

1. Build a held-out, human-labelled query set from representative content with
   exact identifiers, paraphrases, cross-document dependencies, contradictory or
   stale information, and questions with no supporting answer. Include each
   required supporting passage for multi-hop questions, not just one expected
   document. Use synthetic or explicitly approved content for external providers.
2. Compare dense-only, BM25-only, balanced hybrid, graph expansion and reranking
   at the same candidate and context budgets. Record evidence recall, reciprocal
   rank, graded nDCG, complete multi-hop evidence coverage, citation support,
   abstention quality, p50/p95 latency and provider usage. The current Playground
   evaluates a single expected source/text per question; it does not establish
   complete multi-hop coverage or graded relevance.
3. Test personalized graph diffusion against the existing beam. Normalize
   outgoing evidence mass so hubs do not gain weight merely from connectivity;
   compare with cycles, duplicated links, weak bridges and disconnected evidence.
   Apply document eligibility at every step and keep reproducible source paths.
4. Test bounded query decomposition only on questions whose initial evidence is
   insufficient. Compare additional recall with latency, cost, and unsupported
   query drift before enabling it for chatbots or voicebots.

Changes to ranking should be promoted on held-out corpus results. These tests
do not establish ten-million-document capacity or justify increasing current
collection/traversal limits. Saywit's current SQL cosine adapter also does not
call GraphRAG `/retrieve`; this engine change reaches GraphRAG retrieval/chat
consumers, not that adapter's dense-only search automatically.
