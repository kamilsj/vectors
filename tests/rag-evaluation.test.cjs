"use strict";
const { test } = require("node:test");
const assert = require("node:assert/strict");
const helpers = import("../web/rag-evaluation.mjs");

test("whole-set validation rejects invalid unsampled rows and unsupported labels", async () => {
  const { parseEvaluationSet, sampleEvaluationSet } = await helpers;
  const question = { question: "Which source?", expected_source: "manual.pdf#page=1" };
  const rows = Array.from({ length: 1000 }, () => question);
  rows[998] = { ...question, expected_text: "" };
  assert.throws(() => parseEvaluationSet(JSON.stringify(rows)), /Question 999/);
  assert.throws(() => parseEvaluationSet(JSON.stringify([{ ...question, expected_answer: false }])), /unsupported field/);
  assert.throws(() => parseEvaluationSet(JSON.stringify([{ ...question, question: "😀".repeat(1969) }])), /7,872/);
  const valid = parseEvaluationSet("\ufeff" + JSON.stringify(Array.from({ length: 1000 }, () => question)));
  assert.deepEqual(sampleEvaluationSet(valid, 3).map((row) => row.index), [0, 500, 999]);
  assert.throws(() => sampleEvaluationSet(valid, 101), /1–100/);
});

test("exact sources, legacy suffixes and facts must match the same final-ranked hit", async () => {
  const { parseEvaluationSet, scoreEvaluationResult } = await helpers;
  const [legacy, exact] = parseEvaluationSet(JSON.stringify([
    { question: "Which code?", expected_file: "manual.pdf", expected_page: 2, expected_text: "ORBIT" },
    { question: "Which code?", expected_source: "manual.pdf#page=2", expected_text: "ORBIT" },
  ]));
  const hit = (id, source, text) => ({ chunk_id: id, source, text });
  const wrong = [hit("a", "manual.pdf#page=2", "wrong"), hit("b", "other.pdf#page=2", "ORBIT")];
  assert.equal(scoreEvaluationResult(legacy, { hits: wrong }, 5, 10).matched_rank, null);
  const response = { revision: 2, hits: [...wrong, hit("c", "folder/manual.pdf#page=2", "Code ORBIT")] };
  assert.equal(scoreEvaluationResult(legacy, response, 5, 10).matched_rank, 3);
  assert.equal(scoreEvaluationResult(exact, response, 5, 10).matched_rank, null);
  response.hits[2].source = "manual.pdf#page=2";
  assert.equal(scoreEvaluationResult(exact, response, 5, 10).matched_rank, 3);
  assert.throws(() => scoreEvaluationResult(exact, { hits: [wrong[0], wrong[0]] }, 5, 10), /Invalid retrieval response/);
  assert.throws(() => scoreEvaluationResult(exact, { hits: null }, 5, 10), /Invalid retrieval response/);
});

test("metrics count valid misses, exclude errors and report unknown or changed revisions", async () => {
  const { summarizeEvaluation } = await helpers;
  const rows = [2, null, 1, null].map((rank, index) => ({ status: "complete", matched_rank: rank, elapsed_ms: [10, 20, 30, 40][index], revision: 7 }));
  const summary = summarizeEvaluation([...rows, { status: "error" }], 10);
  assert.equal(summary.completed, 4); assert.equal(summary.failed, 1); assert.equal(summary.planned, 10);
  assert.equal(summary.match_rate, .5); assert.equal(summary.mean_reciprocal_rank, .375);
  assert.deepEqual(summary.latency_ms, { median: 25, p95: 40 }); assert.equal(summary.revision_status, "stable");
  rows[0].revision = null; assert.equal(summarizeEvaluation(rows, 4).revision_status, "unknown");
  rows[1].revision = 8; assert.equal(summarizeEvaluation(rows, 4).revision_status, "changed");
  assert.equal(summarizeEvaluation([], 3).match_rate, null);
});

test("bounded evidence retains a late matching passage and marks Unicode excerpts", async () => {
  const { parseEvaluationSet, scoreEvaluationResult } = await helpers;
  const [question] = parseEvaluationSet(JSON.stringify([{ question: "Which source?", expected_source: "right" }]));
  const hits = Array.from({ length: 100 }, (_, index) => ({ chunk_id: String(index), source: index === 99 ? "right" : "wrong", text: "😀".repeat(2001) }));
  const scored = scoreEvaluationResult(question, { hits }, 1, 100);
  assert.equal(scored.matched_rank, 100); assert.equal(scored.hit_count, 100); assert.equal(scored.evidence.length, 6);
  assert.equal(scored.evidence[5].rank, 100); assert.equal(scored.evidence[5].matched, true);
  assert.equal(Array.from(scored.evidence[5].text).length, 2000); assert.equal(scored.evidence[5].truncated, true);
  assert.equal(scored.revision, null);
});

test("reranking provenance preserves actual bounded identity and token usage only", async () => {
  const { parseEvaluationSet, scoreEvaluationResult } = await helpers;
  const [question] = parseEvaluationSet('[{"question":"Which source?","expected_source":"right"}]');
  const score = (reranking) => scoreEvaluationResult(question, { hits: [], reranking }, 1, 10);
  assert.deepEqual(score({ method: "voyage", model: "rerank-2.5", total_tokens: 41, secret: "do not copy" }).reranking,
    { method: "voyage", model: "rerank-2.5", total_tokens: 41 });
  assert.deepEqual(score({ method: "local", model: null, total_tokens: 0 }).reranking,
    { method: "local", model: null, total_tokens: 0 });
  for (const total_tokens of [-1, 1.5, Number.MAX_SAFE_INTEGER + 1, Infinity, "5", undefined]) {
    assert.equal(score({ method: "voyage", model: "rerank-2.5", total_tokens }).reranking.total_tokens, null);
  }
  for (const reranking of [undefined, null, [], {}, { method: "local" }, { method: "voyage", model: "" },
    { method: "x".repeat(65), model: null }, { method: "voyage", model: "x".repeat(129) },
    { method: "voyage", model: "😀".repeat(33) }, { method: "voyage", model: "bad\0model" }]) {
    assert.equal(score(reranking).reranking, null);
  }
  assert.equal(score({ method: "voyage", model: "😀".repeat(32) }).reranking.model, "😀".repeat(32));
});

test("reranking summary detects model and method changes independently of revisions", async () => {
  const { parseEvaluationSet, scoreEvaluationResult, summarizeEvaluation } = await helpers;
  const [question] = parseEvaluationSet('[{"question":"Which source?","expected_source":"right"}]');
  const score = (method, model, total_tokens = 1) => scoreEvaluationResult(question,
    { hits: [], revision: 7, reranking: { method, model, total_tokens } }, 1, 10);
  const local = score("local", null, 0);
  const voyage = score("voyage", "rerank-2.5");
  assert.equal(summarizeEvaluation([local, local], 2).reranking_status, "stable");
  assert.equal(summarizeEvaluation([voyage, score("voyage", "rerank-2.5", 999)], 2).reranking_status, "stable");
  for (const rows of [[voyage, score("voyage", "rerank-2.5-lite")], [local, voyage]]) {
    const summary = summarizeEvaluation(rows, 2);
    assert.equal(summary.reranking_status, "changed");
    assert.equal(summary.revision_status, "stable");
  }
  assert.equal(summarizeEvaluation([voyage, { status: "error", reranking: local.reranking }], 2).reranking_status, "stable");
});

test("missing or unrecognized reranking is unknown without hiding proven changes", async () => {
  const { parseEvaluationSet, scoreEvaluationResult, summarizeEvaluation } = await helpers;
  const [question] = parseEvaluationSet('[{"question":"Which source?","expected_source":"right"}]');
  const score = (reranking) => scoreEvaluationResult(question, { hits: [], revision: 7, reranking }, 1, 10);
  const local = score({ method: "local", model: null, total_tokens: 0 });
  const voyage = score({ method: "voyage", model: "rerank-2.5", total_tokens: 1 });
  for (const row of [score(undefined), score({ method: "voyage", model: null, total_tokens: 0 }),
    score({ method: "future", model: "unknown", total_tokens: 0 }), score({ method: "local", model: "unexpected" })]) {
    assert.equal(summarizeEvaluation([local, row], 2).reranking_status, "unknown");
    assert.equal(summarizeEvaluation([local, voyage, row], 3).reranking_status, "changed");
  }
  assert.equal(summarizeEvaluation([], 0).reranking_status, "unknown");
});
