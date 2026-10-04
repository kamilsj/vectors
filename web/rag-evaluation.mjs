// Deterministic retrieval checks. This module never sends requests or generates answers.
export const EVALUATION_LIMITS = Object.freeze({ fileBytes: 5 * 1024 * 1024, questions: 10000, runQuestions: 100, questionBytes: 7872 });
const bytes = (value) => new TextEncoder().encode(value).length;
const record = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
const text = (value, maximum) => typeof value === "string" && value.trim().length > 0 && !value.includes("\0") && bytes(value) <= maximum;

export function parseEvaluationSet(input) {
  if (typeof input !== "string" || bytes(input) > EVALUATION_LIMITS.fileBytes) throw new Error("Question files must be 5 MiB or smaller.");
  let rows;
  try { rows = JSON.parse(input.replace(/^\uFEFF/, "")); }
  catch { throw new Error("Choose a valid JSON question file."); }
  if (!Array.isArray(rows) || !rows.length || rows.length > EVALUATION_LIMITS.questions) throw new Error("Use a JSON list containing 1–10,000 questions.");
  return rows.map((row, index) => {
    const fail = (message) => { throw new Error(`Question ${index + 1}: ${message}`); };
    if (!record(row) || !text(row.question, EVALUATION_LIMITS.questionBytes)) fail("provide a question of at most 7,872 UTF-8 bytes.");
    if (Object.keys(row).some((key) => !["question", "expected_source", "expected_text", "expected_file", "expected_page"].includes(key))) fail("unsupported field; use question, expected_source and optional expected_text, or the PDF file/page format.");
    let source; let match;
    if (Object.hasOwn(row, "expected_source")) {
      if (!text(row.expected_source, 2048) || Object.hasOwn(row, "expected_file") || Object.hasOwn(row, "expected_page")) fail("provide one exact expected_source, without file/page fields.");
      source = row.expected_source; match = "exact";
    } else {
      if (!text(row.expected_file, 2048) || !Number.isSafeInteger(row.expected_page) || row.expected_page < 1 || row.expected_page > 1000000 || !text(row.expected_text, 65536)) fail("PDF questions need expected_file, a positive expected_page and expected_text.");
      source = `${row.expected_file}#page=${row.expected_page}`; match = "file_page_suffix";
    }
    if (Object.hasOwn(row, "expected_text") && !text(row.expected_text, 65536)) fail("expected_text must be nonempty text of at most 64 KiB.");
    return { index, question: row.question.trim(), expected_source: source, source_match: match, expected_text: row.expected_text ?? null };
  });
}

export function sampleEvaluationSet(questions, limit) {
  if (!Number.isInteger(limit) || limit < 1 || limit > EVALUATION_LIMITS.runQuestions) throw new Error("Test 1–100 questions per run.");
  const count = Math.min(limit, questions.length);
  return Array.from({ length: count }, (_, index) => questions[Math.round(index * (questions.length - 1) / Math.max(1, count - 1))]);
}

function rerankingProvenance(value) {
  if (!record(value) || !text(value.method, 64) || (value.model !== null && !text(value.model, 128))) return null;
  return { method: value.method, model: value.model,
    total_tokens: Number.isSafeInteger(value.total_tokens) && value.total_tokens >= 0 ? value.total_tokens : null };
}

function rerankingIdentity(value) {
  if (!record(value)) return null;
  if (value.method === "local" && value.model === null) return JSON.stringify(["local", null]);
  if (value.method === "voyage" && text(value.model, 128)) return JSON.stringify(["voyage", value.model]);
  // A skipped Voyage call returns a null model; it cannot confirm model identity.
  return null;
}

export function scoreEvaluationResult(question, response, elapsed, topK) {
  const invalid = () => { throw new Error("Invalid retrieval response. Evaluation stopped without retrying."); };
  if (!record(response) || !Array.isArray(response.hits) || response.hits.length > topK || !Number.isFinite(elapsed) || elapsed < 0) invalid();
  const ids = new Set();
  for (const hit of response.hits) {
    if (!record(hit) || !text(hit.chunk_id, 1024) || typeof hit.source !== "string" || typeof hit.text !== "string" || ids.has(hit.chunk_id)) invalid();
    ids.add(hit.chunk_id);
  }
  const matches = (hit) => (hit.source === question.expected_source || (question.source_match === "file_page_suffix" && hit.source.endsWith("/" + question.expected_source)))
    && (question.expected_text === null || hit.text.includes(question.expected_text));
  const index = response.hits.findIndex(matches);
  const matchedRank = index < 0 ? null : index + 1;
  const evidence = response.hits.flatMap((hit, position) => position < 5 || position === index ? [{
    chunk_id: hit.chunk_id, document_id: typeof hit.document_id === "string" ? hit.document_id : "",
    title: typeof hit.title === "string" ? hit.title : "", source: hit.source,
    text: Array.from(hit.text).slice(0, 2000).join(""), truncated: Array.from(hit.text).length > 2000,
    rank: position + 1, matched: matches(hit),
  }] : []);
  const numericFields = (value) => record(value) ? Object.fromEntries(Object.entries(value).filter(([, number]) => typeof number === "number" && Number.isFinite(number) && number >= 0)) : null;
  return { ...question, status: "complete", matched_rank: matchedRank, elapsed_ms: Math.round(elapsed * 1000) / 1000,
    revision: Number.isSafeInteger(response.revision) && response.revision >= 0 ? response.revision : null,
    candidate_count: Number.isSafeInteger(response.candidate_count) && response.candidate_count >= 0 ? response.candidate_count : null,
    hit_count: response.hits.length, embedding_usage: numericFields(response.embedding_usage),
    reranking: rerankingProvenance(response.reranking), timings: numericFields(response.timings), evidence };
}

export function summarizeEvaluation(results, planned) {
  const completed = results.filter((row) => row.status === "complete");
  const latencies = completed.map((row) => row.elapsed_ms).sort((a, b) => a - b);
  const count = completed.length; const middle = Math.floor(count / 2);
  const revisions = new Set(completed.map((row) => row.revision).filter((revision) => revision !== null));
  const reranking = completed.map((row) => rerankingIdentity(row.reranking));
  const rerankers = new Set(reranking.filter((identity) => identity !== null));
  return { completed: count, failed: results.filter((row) => row.status === "error").length, planned,
    match_rate: count ? completed.filter((row) => row.matched_rank !== null).length / count : null,
    mean_reciprocal_rank: count ? completed.reduce((sum, row) => sum + (row.matched_rank ? 1 / row.matched_rank : 0), 0) / count : null,
    latency_ms: { median: count ? count % 2 ? latencies[middle] : (latencies[middle - 1] + latencies[middle]) / 2 : null, p95: count ? latencies[Math.ceil(.95 * count) - 1] : null },
    revision_status: revisions.size > 1 ? "changed" : !count || completed.some((row) => row.revision === null) ? "unknown" : "stable",
    reranking_status: rerankers.size > 1 ? "changed" : !count || reranking.includes(null) ? "unknown" : "stable" };
}
