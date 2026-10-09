# Test RAG with PDF collections

Vectors opens in **Playground**, with **Chat**, **Documents**, and **Graph**
tabs. Start with
a small representative batch, measure retrieval, then grow the corpus while
watching capacity. The importer processes text-selectable PDFs in your browser;
the server receives extracted text, page references and metadata. Original PDF
files are not stored by Vectors.
Use a current Chromium browser over localhost or HTTPS; file identities use the
browser's secure cryptography API. Folder selection depends on browser support.

## Set up a collection

Start the local 0.11.2 build from the repository, then open
<http://127.0.0.1:8081>:

```sh
cargo run --release --bin vectors-server -- --port 8081 --data-dir ./vectors-data/pdf-lab
```

1. Configure OpenAI or
   Voyage embeddings in **Settings**. Put persistent keys in the private
   `.env.local` file or use `--env-file`; keys entered in Settings last for the
   server process. See [local configuration](EMBEDDINGS_AND_ADMIN.md).
2. Create a RAG collection in **Data**, or select an existing one in
   **Playground**. Use one embedding model/dimension profile consistently.
3. For an initial bulk throughput test, set semantic neighbors to **0** when
   creating the collection. This avoids exact semantic-neighbor scans for every
   imported chunk. A separate small collection with semantic neighbors enabled
   lets you compare graph expansion against direct retrieval.
4. If the collection declares required metadata fields, fill those values in
   the document editor before starting the import. Unique fields need distinct
   values for each document; a shared value cannot serve an entire batch.

The following creates a simple collection with no extra required fields:

```sh
curl http://127.0.0.1:8081/v1/graph/collections \
  -H 'Content-Type: application/json' \
  -d '{"name":"pdf_lab","semantic_neighbors":0,"semantic_threshold":0.8}'
```

For authenticated servers, include the bearer token in requests. Never put
provider keys in PDF files or the browser console.

## Upload files and inspect retrieval

In **Playground → Documents**, select multiple files or a folder. Start the
queue and keep the browser tab open. Files are extracted and sent sequentially,
so the importer does not allocate the whole corpus or issue concurrent paid
embedding requests. Pause stops scheduling new work after the active request.
Review failed files before using the explicit retry action. A timed-out write
may already have completed; retries reuse deterministic identifiers and the
server skips identical documents without embedding again.

Each PDF page becomes a document, with oversized pages split into smaller
parts. Citations retain `filename.pdf#page=N`; byte offsets refer to extracted
UTF-8 text in that page/part, **not bytes inside the original PDF file**. Page
parts are separate documents, so `max_per_document` is a page/part cap, not a
whole-PDF cap. Consecutive PDF pages do not automatically receive adjacency
edges. Semantic links or explicit relationships can connect them.

Image-only pages are reported as needing OCR. OCR, table reconstruction and
layout-perfect reading order are not included. Inspect a few extracted pages
from your own documents before committing to a large import. The queue lives
in the current tab; reselecting the same files after reload allows unchanged
pages to be skipped, but completed database writes remain after closing it.

Use **Chat** to ask a question. Open **Settings** to choose **Retrieve sources**
or **Answer with sources**, adjust retrieval, or filter documents. Answer mode
uses the server's OpenAI key and a configurable model
(default `gpt-4.1-mini`). The same OpenAI key works when Voyage is selected for
embeddings. Each turn retrieves fresh evidence. **Current question** searches
the question unchanged; name its subject explicitly. **Recent conversation**
can make an additional OpenAI call to turn a follow-up into a standalone search
query. Inspect the effective query, rewrite usage and evidence in each turn.
**Source excerpts** requires current-source citations and exact quotations;
**Voice script** provides concise text to copy into a speech service. These
mechanical checks do not prove every factual claim. See the
[chatbot and voicebot guide](CHATBOTS.md) for API options and answer evaluation.

Choose one of four retrieval strategies:

| Strategy | Behavior |
| --- | --- |
| Graph + hybrid | Semantic and keyword matches, with connected passages. |
| Hybrid | Semantic and keyword matches, without graph expansion. |
| Vector | Semantic matches, without keyword candidates or graph expansion. |
| Keyword | Keyword matches, without query embeddings or graph expansion. |

**Keyword + Local ranking + Retrieve sources + Current question** makes no provider calls and
works on existing collections without an active embedding key. Ingestion still
requires the collection's embedding provider. Keyword results have no query
cosine similarity; stored vectors may still diversify the selected passages.
Voyage reranking and answer generation use their providers when selected.

To compare settings, ask a question, change the strategy or result limits, then
click **Run again**. It repeats the last question with the current settings and
the original run's conversation snapshot. **Compare** shows source overlap, added/removed passages,
retrieval timing, and each run's configuration. Comparisons require the same
question, collection, mode, and document filters. A database-revision warning
identifies writes between runs; unrelated table writes can also advance this
revision. Existing runs retain their
original settings and filters; new searches only reuse history from the same
document scope. The latest ten runs are kept in the current browser session.

Expand a turn's inspection panel to see:

- Source passages and `[S1]` citation labels; unknown or missing generated
  citation labels produce warnings, not a claim of factual verification.
- Vector similarity, keyword score, fused rank, optional reranking and final
  selection score, plus actual initial traversal seed identifiers.
- Graph discovery paths, filters, database revision, candidate count and context
  budget. A direct-match flag alone does not identify a traversal seed.
- Measured embedding, search, reranking, selection and answer-generation timing,
  with available provider token usage.

This is observable retrieval behavior, not a model's private reasoning. The
server sends only selected passages and bounded conversation history for
answer generation, using the [OpenAI Responses API](https://developers.openai.com/api/docs/guides/text)
with response storage disabled. The original PDFs stay in the browser.

## Chat API

The interface calls the same authenticated graph API available to your clients:

```sh
curl http://127.0.0.1:8081/v1/graph/collections/pdf_lab/chat \
  -H 'Content-Type: application/json' \
  -d '{"text":"What is the recovery code for ATLAS-00001?",
       "mode":"answer","model":"gpt-4.1-mini","history":[],
       "retrieval":{"max_hops":1,"max_seeds_per_document":2,"max_results":5}}'
```

`retrieval` accepts the existing `/retrieve` options except `text`; put the
question at the top level. Use `mode: "retrieve"` to skip answer generation.
The response contains `answer`, `retrieval`, `citations`, `generation`,
`timings` and `warnings`. History entries contain `role` (`user` or `assistant`)
and `content`. History is limited to 20 messages, 8 KiB per message and 32 KiB
total; context is limited to 64 KiB. Answer workflows have four shared admission
slots, a 60-second generation deadline and a 2,048-token output budget. Generation
errors remain explicit; the server does not disguise failed answers as success.

## Evaluate a question set in the Playground

Choose **Evaluate** in Chat to open the evaluation panel. Import the generated
`questions.json` or download the example and replace it with your own questions:

```json
[
  {
    "question": "What is the retention period?",
    "expected_source": "policy.pdf#page=2",
    "expected_text": "30 days"
  }
]
```

`expected_source` matches the returned source exactly. Optional `expected_text`
must occur literally, with matching case, in that same passage. The generated
PDF format (`expected_file`, `expected_page`, `expected_text`) also accepts
folder prefixes; use an exact source when different folders contain the same
filename. Unsupported fields and invalid rows anywhere in the file are rejected
before any retrieval request.

Set the number of questions (default 20, maximum 100 per run), then choose
**Run evaluation**. Sampling is evenly spread across up to 10,000 imported
questions in a file of at most 5 MiB. The run freezes the selected collection,
retrieval strategy, result limits, reranking choice and document filters. Queries run
sequentially through `/retrieve`; no answers are generated. Keyword + Local
ranking makes no provider calls; other settings can incur provider charges.

**Pause** finishes the current request. **Resume** keeps the same questions and
settings. To change a paused plan, clear it and import the question set again.
Failed requests stop the run without retrying. Changing filters cancels pending
evaluation results; changing collection clears the report, and changing the
connection clears the question set as well. An already submitted provider
request may still finish after cancellation.

The panel reports **Match rate@K**, **MRR@K**, and median/p95 elapsed time. A miss
contributes zero to the first two metrics; failed and unrun questions are
excluded, with completed/planned and failure counts shown separately. These
checks measure retrieval against the supplied expectations, not answer quality.
Database-revision changes or missing revision information are flagged. Each
completed result also records the server's actual reranking method, model and
token usage. The report's `reranking_status` is `stable`, `changed`, or `unknown`:
changing reranking models during a pause or run is detected even if the database
revision stays the same. Local ranking has a known `local` method and no model.
Missing provenance, an unrecognized method, or a Voyage response without a model
(including a request with no candidates to rerank) leaves identity unconfirmed.
Token counts alone do not change reranking identity. Use
**Export report** to save full run settings, sampled questions, per-question
results, usage and timings. Evidence includes excerpts from the first five
passages plus the first matching passage; matching uses the full retrieved text.

## Generate a repeatable corpus

No third-party Python packages or provider calls are needed to create fixtures:

```sh
python3 scripts/rag_pdf_corpus.py create --files 1000 --output /tmp/pdf-rag-1000
```

The command creates 1,000 two-page PDFs under `/tmp/pdf-rag-1000/pdfs` and
2,000 known-answer questions in `questions.json`. Choose that `pdfs` folder in
the importer. The output directory must be new, preventing accidental overwrite.

Try: **What is the recovery authorization code for ATLAS-00001?** The expected
answer is **ORBIT-07919**, cited from `manual-00001.pdf#page=1`. Then ask how many
days that project retains backups, explicitly naming **ATLAS-00001**; page 2
states **21 days**. Other manuals contain similar wording but different facts,
so source selection matters.

After import finishes, measure source retrieval with a sequential sample of
questions spread across the corpus:

```sh
python3 scripts/rag_pdf_corpus.py evaluate \
  --url http://127.0.0.1:8081 \
  --collection pdf_lab --questions /tmp/pdf-rag-1000/questions.json \
  --limit 20 --top-k 10 --hops 0 --output /tmp/rag-direct.json
python3 scripts/rag_pdf_corpus.py evaluate \
  --url http://127.0.0.1:8081 \
  --collection pdf_lab --questions /tmp/pdf-rag-1000/questions.json \
  --limit 20 --top-k 10 --hops 1 --output /tmp/rag-graph.json
```

Evaluation reads `VECTORS_API_TOKEN` for authentication and calls the configured
embedding provider; normal provider costs apply. It does not generate answers
or silently retry requests. Reports contain source-and-answer recall at K,
mean reciprocal rank, median/p95 end-to-end latency, token usage and revisions.
Compare runs on the same completed collection; a changing revision invalidates
a controlled comparison. Graph-on and graph-off may match when a collection has
no edges. Simple synthetic fixtures verify ingestion/provenance, not production
answer quality. Add your own expected questions, including unanswerable ones,
and manually check faithfulness and citations.

## Capacity and scale

`GET /v1/graph/collections/{collection}/capacity` reports current usage and hard
limits before paid work. The UI displays collection capacity; import still
validates each document at commit, including changes by other clients.

| Resource | Limit per collection |
| --- | ---: |
| Chunks | 10,000 |
| Directed edges | 340,000 |
| Vector elements | 33,554,432 |
| Accounted source/chunk/context/relationship text | 64 MiB |
| Extracted text in one document | 1 MiB |
| Chunks in one document | 256 |

PDF count is not the capacity metric: a thousand long documents may exceed
these limits. Pages, overlap, dimensions, metadata and links all affect usage.
Split corpora into meaningful collections before capacity is reached; chat
currently searches one selected collection, not a federation of collections.
Use a separate server/data directory for competing bulk import jobs because
writes share a catalog revision and parallel imports may conflict after
provider work has already occurred.

The storage engine still copies catalog state for atomic writes, uses exact
vector search, and holds a write lock during graph ingestion. Increasing a
constant alone would not establish support for millions of pages. Measure your
hardware and workload with progressively larger sets and stop when import time
or query latency becomes unacceptable.

A provider-free engine benchmark exercises durable page-like ingestion,
restart recovery and checked retrieval with realistic vector dimensions:

```sh
cargo run --release --example benchmark_pdf_rag -- 1000 1536
```

Its timings exclude PDF extraction, provider/network latency, semantic linking
and concurrent query contention. Browser regression tests separately exercise
real PDF extraction and the upload/chat lifecycle with controlled API responses.

Local macOS arm64 checks at 1,536 dimensions reached 1,000 and 10,000 chunks and
returned the expected first source for all 20 queries after durable restart at
each size. See the [recorded measurements](benchmarks/pdf-rag-durable-2026-09-27.json)
and [benchmark notes](BENCHMARKS.md#durable-pdf-like-rag-workload). These are
synthetic engine checks, not a throughput promise for your PDF/provider mix.
