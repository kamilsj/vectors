"use strict";

const { test, expect } = require("@playwright/test");
const fs = require("node:fs/promises");
const reply = (route, body, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
const deferred = () => { let resolve; const promise = new Promise((done) => { resolve = done; }); return { promise, resolve }; };
const profile = { provider: "openai", model: "text-embedding-3-small", dimensions: 3, context_format_version: 1 };
const collection = (name) => ({ config: { name, profile, semantic_neighbors: 0, semantic_threshold: .8 }, document_columns: [{ name: "category", data_type: "TEXT", nullable: true, unique: false }], revision: 7, document_count: 3, chunk_count: 10, edge_count: 0, tables: {} });
const hit = (source = "manual.pdf#page=2", text = "The recovery code is ORBIT-42.", id = "chunk-one") => ({ chunk_id: id, document_id: `doc-${id}`, title: "Operations manual", source, text, ordinal: 0, start_byte: 0, end_byte: Buffer.byteLength(text), seed: true, depth: 0, similarity: .8, lexical_score: 1, fusion_score: .02, selection_score: .6 });
const response = (hits = [hit()], revision = 7) => ({ collection: "knowledge", revision, hits, edges: [], traversal_seed_ids: [], candidate_count: hits.length, context_bytes: hits.reduce((sum, item) => sum + Buffer.byteLength(item.text), 0), lexical_cache_hit: true, truncated: false, reranking: { method: "local", model: null }, embedding_usage: { total_tokens: 3 }, timings: { embedding_ms: 1, search_ms: 2, reranking_ms: 0, selection_ms: 1, generation_ms: 0, total_ms: 4 } });
const questions = (count) => Array.from({ length: count }, (_, index) => ({ question: `Question ${index}`, expected_source: "manual.pdf#page=2", expected_text: "ORBIT-42" }));
const retrievals = (fixture) => fixture.calls.filter((call) => call.path.endsWith("/retrieve"));

async function workspace(page, { configured = true, dimensions = 3, rerankingConfigured = false } = {}) {
  const fixture = { calls: [], collections: [collection("knowledge"), collection("archive")], respond: () => response() };
  await page.addInitScript(() => sessionStorage.setItem("vectors.apiToken", "synthetic-evaluation-token"));
  page.on("request", (request) => {
    const path = new URL(request.url()).pathname;
    if (path.startsWith("/v1/")) fixture.calls.push({ path, method: request.method(), body: request.postData() ? request.postDataJSON() : null });
  });
  await page.route("**/healthz", (route) => reply(route, { status: "ok", version: "0.10.0", storage: "memory" }));
  await page.route("**/v1/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    const body = route.request().postData() ? route.request().postDataJSON() : null;
    if (path === "/v1/tables") return reply(route, { revision: 7, tables: [] });
    if (path === "/v1/settings/embeddings") return reply(route, { ...profile, dimensions, configured, generation_configured: false, persistence: "memory", timeout_seconds: 60, batch_size: 32, max_concurrent_requests: 4, providers: [{ id: "openai", label: "OpenAI", configured, models: [{ id: profile.model, dimensions: [3], default_dimensions: 3 }] }] });
    if (path === "/v1/settings/reranking") return reply(route, { configured: rerankingConfigured, model: "rerank-2.5", timeout_seconds: 60, models: ["rerank-2.5"] });
    if (path === "/v1/graph/collections") {
      if (route.request().method() === "POST") { const added = collection(body.name); fixture.collections.push(added); return reply(route, added); }
      return reply(route, fixture.collections);
    }
    if (path.endsWith("/graph")) return reply(route, { collection: path.split("/")[4], revision: 7, nodes: [], edges: [], total_nodes: 0, total_edges: 0, offset: 0, limit: 100, truncated: false });
    if (path.endsWith("/retrieve")) return reply(route, await fixture.respond(body));
    return reply(route, { error: { code: "unexpected_request", message: "Unexpected evaluation test request" } }, 404);
  });
  await page.goto("/");
  await expect(page.locator("#status-label")).toHaveText("Connected");
  await expect(page.locator("#graph-collection")).toHaveValue("knowledge");
  return fixture;
}

async function openEvaluation(page) {
  await page.locator('[data-playground-tab="chat"]').click();
  if (!(await page.locator("#playground-evaluation").evaluate((element) => element.open))) await page.locator("#playground-evaluate-open").click();
}
async function loadDataset(page, rows, name = "questions.json") {
  await openEvaluation(page);
  await page.locator("#evaluation-file").setInputFiles({ name, mimeType: "application/json", buffer: Buffer.from(JSON.stringify(rows)) });
  await expect(page.locator("#evaluation-dataset")).toContainText(name);
}
async function waitForRows(page, count) {
  await expect(page.locator("#evaluation-results [data-evaluation-row]")).toHaveCount(count);
  await expect(page.locator("#evaluation-pause")).toBeDisabled();
  await expect(page.locator("#evaluation-export")).toBeEnabled();
}
async function exportReport(page) {
  const pending = page.waitForEvent("download");
  await page.locator("#evaluation-export").click();
  const download = await pending;
  expect(download.suggestedFilename()).toMatch(/\.json$/);
  const text = await fs.readFile(await download.path(), "utf8");
  expect(text).not.toContain("synthetic-evaluation-token");
  return JSON.parse(text);
}
async function openSettings(page) {
  if (await page.locator("#playground-settings").isHidden()) await page.locator("#playground-settings-toggle").click();
}
async function addFilter(page, value) {
  await openSettings(page);
  if (!(await page.locator("#graph-filters-panel").evaluate((element) => element.open))) await page.locator("#graph-filters-panel > summary").click();
  await page.locator("#graph-filter-add").click();
  const row = page.locator("[data-graph-filter-row]").last();
  await row.locator("[data-filter-column]").selectOption("category");
  await row.locator("[data-filter-value]").fill(value);
  return row;
}

test("batch matching requires the expected source and fact in the same ranked passage", async ({ page }) => {
  const fixture = await workspace(page);
  await expect(page.locator("#playground-evaluation")).not.toHaveAttribute("open", "");
  const rows = [
    { question: "Legacy PDF source", expected_file: "manual.pdf", expected_page: 2, expected_text: "ORBIT-42" },
    { question: "Exact folder and fact", expected_source: "a/manual.pdf#page=2", expected_text: "ORBIT-42" },
    { question: "Source alone", expected_source: "manual.pdf#page=1" },
    { question: "Wrong file prefix", expected_file: "manual.pdf", expected_page: 2, expected_text: "ORBIT-42" },
  ];
  const answers = [
    response([hit("manual.pdf#page=1", "ORBIT-42", "wrong-page"), hit("folder/manual.pdf#page=2", "Code ORBIT-42", "matching")]),
    response([hit("b/manual.pdf#page=2", "ORBIT-42", "other-folder"), hit("a/manual.pdf#page=2", "wrong code", "right-source"), hit("manual.pdf#page=2", "ORBIT-42", "wrong-source")]),
    response([hit("manual.pdf#page=1", "Any passage", "source-only")]),
    response([hit("wrong-manual.pdf#page=2", "ORBIT-42", "wrong-prefix")]),
  ];
  fixture.respond = (body) => answers[rows.findIndex((row) => row.question === body.text)];
  await loadDataset(page, rows); expect(retrievals(fixture)).toHaveLength(0);
  await page.locator("#evaluation-start").click(); await waitForRows(page, 4);
  const report = await exportReport(page);
  expect(report).toMatchObject({ schema_version: 1, kind: "retrieval_evaluation", collection: "knowledge", planned: 4, status: "complete", summary: { completed: 4, failed: 0, planned: 4, match_rate: .5, mean_reciprocal_rank: .375, revision_status: "stable" } });
  expect(report.results.map((item) => item.matched_rank)).toEqual([2, null, 1, null]);
  expect(report.results.map((item) => item.source_match)).toEqual(["file_page_suffix", "exact", "exact", "file_page_suffix"]);
  const elapsed = report.results.map((item) => item.elapsed_ms).sort((a, b) => a - b);
  expect(report.summary.latency_ms.median).toBeCloseTo((elapsed[1] + elapsed[2]) / 2, 3);
  expect(report.summary.latency_ms.p95).toBeCloseTo(elapsed[3], 3);
  expect(retrievals(fixture)).toHaveLength(4);
  for (const call of retrievals(fixture)) { expect(call.body).not.toHaveProperty("expected_source"); expect(call.body).not.toHaveProperty("expected_text"); }
  expect(fixture.calls.some((call) => call.path.endsWith("/chat"))).toBe(false);
});

test("the whole dataset is validated even when an invalid row is outside the sample", async ({ page }) => {
  const fixture = await workspace(page); await openEvaluation(page);
  const rows = questions(1000); rows[1] = { question: "Unsampled but invalid" };
  await page.locator("#evaluation-file").setInputFiles({ name: "invalid-tail.json", mimeType: "application/json", buffer: Buffer.from(JSON.stringify(rows)) });
  await expect(page.locator("#evaluation-status")).toContainText(/source|expected|row|question/i);
  await expect(page.locator("#evaluation-start")).toBeDisabled();
  expect(retrievals(fixture)).toHaveLength(0);
});

test("the downloadable template can be imported without issuing retrieval requests", async ({ page }) => {
  const fixture = await workspace(page); await openEvaluation(page);
  const pending = page.waitForEvent("download"); await page.locator("#evaluation-template").click();
  const download = await pending;
  const rows = JSON.parse(await fs.readFile(await download.path(), "utf8"));
  expect(Array.isArray(rows)).toBe(true); expect(rows.length).toBeGreaterThan(0);
  await loadDataset(page, rows, download.suggestedFilename());
  await expect(page.locator("#evaluation-start")).toBeEnabled();
  expect(retrievals(fixture)).toHaveLength(0);
});

test("import and sample bounds reject oversized plans before retrieval", async ({ page }) => {
  const fixture = await workspace(page); await openEvaluation(page);
  for (const [name, buffer, error] of [
    ["broken.json", Buffer.from("not JSON"), /JSON/i],
    ["too-many.json", Buffer.from(JSON.stringify(questions(10001))), /10[,.]?000|10000/],
    ["too-large.json", Buffer.alloc(5 * 1024 * 1024 + 1, 32), /5.*MiB|5.*MB|large/i],
    ["long-question.json", Buffer.from(JSON.stringify([{ question: "x".repeat(8192), expected_source: "manual.pdf#page=2" }])), /question|bytes|length/i],
  ]) {
    await page.locator("#evaluation-file").setInputFiles({ name, mimeType: "application/json", buffer });
    await expect(page.locator("#evaluation-status")).toContainText(error);
    expect(retrievals(fixture)).toHaveLength(0);
  }
  await loadDataset(page, questions(1));
  for (const limit of ["0", "101", "1.5"]) {
    await page.locator("#evaluation-limit").fill(limit); await page.locator("#evaluation-start").click();
    await expect(page.locator("#evaluation-status")).toContainText(/1.*100|whole|sample/i);
    expect(retrievals(fixture)).toHaveLength(0);
  }
});

test("sampling spans the full dataset and keyword evaluation needs no provider key", async ({ page }) => {
  const fixture = await workspace(page, { configured: false, dimensions: 2 });
  await openSettings(page); await page.locator("#playground-strategy").selectOption("keyword");
  await loadDataset(page, questions(101)); await page.locator("#evaluation-limit").fill("5");
  await page.locator("#evaluation-start").click(); await waitForRows(page, 5);
  expect(retrievals(fixture).map((call) => call.body.text)).toEqual(["Question 0", "Question 25", "Question 50", "Question 75", "Question 100"]);
  for (const call of retrievals(fixture)) expect(call.body).toMatchObject({ vector_weight: 0, lexical_weight: 1, max_hops: 0 });
  expect((await exportReport(page)).results.map((item) => item.index)).toEqual([0, 25, 50, 75, 100]);
});

test("pause finishes the active request and resume uses the same immutable plan", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await openSettings(page); await page.locator("#playground-strategy").selectOption("keyword");
  await page.locator("#graph-result-limit").fill("5"); await addFilter(page, "Guide");
  await loadDataset(page, questions(3));
  await page.route("**/retrieve", async (route) => { if (!entered) { entered = true; await gate.promise; } await route.fallback(); });
  await page.locator("#evaluation-start").click(); await expect.poll(() => entered).toBe(true);
  await expect(page.locator("#evaluation-start")).toBeDisabled();
  await page.locator("#evaluation-form").evaluate((form) => form.requestSubmit());
  await page.locator("#evaluation-pause").click(); gate.resolve();
  await expect(page.locator("#evaluation-start")).toHaveText(/resume/i); await expect(page.locator("#evaluation-start")).toBeEnabled();
  expect(retrievals(fixture)).toHaveLength(1);
  const paused = await exportReport(page); expect(paused.status).toBe("paused"); expect(paused.summary.completed).toBe(1);
  // Even a programmatic change cannot rewrite a paused plan's captured options.
  await page.locator("#playground-strategy").evaluate((element) => { element.value = "vector"; });
  await page.locator("#evaluation-start").click(); await waitForRows(page, 3);
  for (const call of retrievals(fixture)) expect(call.body).toMatchObject({ max_results: 5, max_hops: 0, vector_weight: 0, lexical_weight: 1, document_filters: [{ column: "category", operator: "eq", value: "Guide" }] });
  expect(retrievals(fixture).map((call) => call.body.text)).toEqual(["Question 0", "Question 1", "Question 2"]);
  expect((await exportReport(page)).options).toMatchObject({ max_results: 5, vector_weight: 0, reranker: "local", document_filters: [{ column: "category", operator: "eq", value: "Guide" }] });
});

for (const [initialStrategy, initialReranker, nextStrategy, nextReranker] of [
  ["keyword", "local", "vector", "voyage"],
  ["vector", "voyage", "keyword", "local"],
]) {
  test(`completed ${initialStrategy} reports keep their scope while the next ${nextStrategy} run previews provider use`, async ({ page }) => {
    const fixture = await workspace(page, { rerankingConfigured: true });
    fixture.respond = () => ({ ...response(), reranking: { method: initialReranker, model: initialReranker === "voyage" ? "rerank-2.5" : null } });
    await openSettings(page);
    await page.locator("#playground-strategy").selectOption(initialStrategy);
    await page.locator("#graph-reranker").selectOption(initialReranker);
    await loadDataset(page, questions(1)); await page.locator("#evaluation-start").click(); await waitForRows(page, 1);
    const original = await exportReport(page);
    await page.locator("#playground-strategy").selectOption(nextStrategy);
    await page.locator("#graph-reranker").selectOption(nextReranker);
    const label = (strategy) => strategy === "keyword" ? "Keyword" : "Semantic";
    const ranking = (reranker) => reranker === "local" ? "Local ranking" : "Voyage reranking";
    await expect(page.locator("#evaluation-settings")).toContainText(`Next run · knowledge · ${label(nextStrategy)}`);
    await expect(page.locator("#evaluation-settings")).toContainText(ranking(nextReranker));
    await expect(page.locator("#evaluation-privacy")).toContainText(nextStrategy === "keyword" ? "No provider calls" : "configured providers");
    await expect(page.locator(".evaluation-report-settings")).toContainText(`Report · knowledge · ${label(initialStrategy)}`);
    await expect(page.locator(".evaluation-report-settings")).toContainText(ranking(initialReranker));
    expect(await exportReport(page)).toEqual(original);
    expect(retrievals(fixture)).toHaveLength(1);
  });
}

test("inspected evidence remains open and focused as later evaluation results arrive", async ({ page }) => {
  const fixture = await workspace(page); const second = deferred(); const third = deferred();
  fixture.respond = async (body) => {
    if (body.text === "Question 1") await second.promise;
    if (body.text === "Question 2") await third.promise;
    return response();
  };
  await loadDataset(page, questions(3)); await page.locator("#evaluation-start").click();
  await expect.poll(() => retrievals(fixture).length).toBe(2);
  const evidence = page.locator('#evaluation-results [data-evaluation-row="0"] details');
  const summary = evidence.locator("summary");
  await summary.click(); await expect(summary).toBeFocused();
  await expect(evidence).toHaveAttribute("open", "");
  await expect(evidence).toContainText("ORBIT-42");
  second.resolve();
  await expect(page.locator("#evaluation-results [data-evaluation-row]")).toHaveCount(2);
  await expect.poll(() => retrievals(fixture).length).toBe(3);
  await expect(evidence).toHaveAttribute("open", ""); await expect(summary).toBeFocused();
  third.resolve(); await waitForRows(page, 3);
  await expect(evidence).toHaveAttribute("open", ""); await expect(summary).toBeFocused();
  await expect(evidence).toContainText("ORBIT-42");
});

test("a failed request stops the batch without retry and leaves an honest partial report", async ({ page }) => {
  const fixture = await workspace(page); await loadDataset(page, questions(3)); let attempt = 0;
  await page.route("**/retrieve", (route) => { attempt += 1; return attempt === 2 ? reply(route, { error: { code: "database_busy", message: "Capacity busy" } }, 503) : route.fallback(); });
  await page.locator("#evaluation-start").click(); await waitForRows(page, 2);
  const report = await exportReport(page);
  expect(retrievals(fixture)).toHaveLength(2);
  expect(report).toMatchObject({ status: "failed", planned: 3, summary: { completed: 1, failed: 1, planned: 3, match_rate: 1, mean_reciprocal_rank: 1 } });
  expect(report.results.map((row) => row.status)).toEqual(["complete", "error"]);
  expect(report.results[1].error).toContain("Capacity busy");
  await expect(page.locator("#evaluation-status")).toContainText(/stopp|fail|busy/i);
});

for (const [name, badResponse] of [
  ["missing hits", { revision: 7 }],
  ["non-array hits", { hits: {} }],
  ["duplicate passage identifiers", response([hit(), hit()])],
]) {
  test(`malformed retrieval with ${name} is an error rather than a scored miss`, async ({ page }) => {
    const fixture = await workspace(page); fixture.respond = () => badResponse;
    await loadDataset(page, questions(2)); await page.locator("#evaluation-start").click(); await waitForRows(page, 1);
    const report = await exportReport(page);
    expect(report).toMatchObject({ status: "failed", summary: { completed: 0, failed: 1, match_rate: null, mean_reciprocal_rank: null } });
    expect(retrievals(fixture)).toHaveLength(1);
  });
}

test("empty hits are a miss and missing or changing database revisions are disclosed", async ({ page }) => {
  const fixture = await workspace(page); let index = 0;
  fixture.respond = () => [response([], 7), response([hit()], 8)][index++];
  await loadDataset(page, questions(2)); await page.locator("#evaluation-start").click(); await waitForRows(page, 2);
  expect((await exportReport(page)).summary).toMatchObject({ completed: 2, match_rate: .5, mean_reciprocal_rank: .5, revision_status: "changed" });
  await expect(page.locator("#evaluation-metrics")).toContainText(/revision|changed/i);
  await page.locator("#evaluation-clear").click();
  fixture.respond = () => { const result = response(); delete result.revision; return result; };
  await loadDataset(page, questions(1)); await page.locator("#evaluation-start").click(); await waitForRows(page, 1);
  expect((await exportReport(page)).summary.revision_status).toBe("unknown");
});

test("changed reranking models are exported and disclosed while missing metadata stays unknown", async ({ page }) => {
  const fixture = await workspace(page, { rerankingConfigured: true }); let index = 0;
  const missing = response(); delete missing.reranking;
  fixture.respond = () => [
    { ...response(), reranking: { method: "voyage", model: "rerank-2.5", total_tokens: 12, api_key: "synthetic-provenance-secret" } },
    { ...response(), reranking: { method: "voyage", model: "rerank-2.5-lite", total_tokens: 11 } },
    missing,
  ][index++];
  await openSettings(page); await page.locator("#graph-reranker").selectOption("voyage");
  await loadDataset(page, questions(3)); await page.locator("#evaluation-start").click(); await waitForRows(page, 3);
  const report = await exportReport(page);
  expect(report.summary).toMatchObject({ completed: 3, failed: 0, match_rate: 1, revision_status: "stable", reranking_status: "changed" });
  expect(report.results.map((row) => row.reranking)).toEqual([
    { method: "voyage", model: "rerank-2.5", total_tokens: 12 },
    { method: "voyage", model: "rerank-2.5-lite", total_tokens: 11 },
    null,
  ]);
  expect(JSON.stringify(report)).not.toContain("synthetic-provenance-secret");
  await expect(page.locator("#evaluation-metrics .evaluation-warning")).toContainText(/reranking.*changed/i);
  await page.locator("#evaluation-results details > summary").first().click();
  await expect(page.locator("#evaluation-results details").first()).toContainText(/Ranking: voyage.*rerank-2\.5/);
  await page.locator("#evaluation-clear").click(); fixture.respond = () => missing;
  await loadDataset(page, questions(1)); await page.locator("#evaluation-start").click(); await waitForRows(page, 1);
  expect((await exportReport(page)).summary).toMatchObject({ completed: 1, failed: 0, match_rate: 1, reranking_status: "unknown" });
  await expect(page.locator("#evaluation-metrics .evaluation-warning")).toContainText(/reranking.*unavailable/i);
});

test("a match below the evidence preview still counts and retains its bounded evidence", async ({ page }) => {
  const fixture = await workspace(page);
  const hits = Array.from({ length: 10 }, (_, index) => hit(index === 6 ? "manual.pdf#page=2" : `other-${index}.pdf`, index === 6 ? `ORBIT-42 ${"x".repeat(4000)}` : "Other source", `chunk-${index}`));
  fixture.respond = () => response(hits);
  await loadDataset(page, questions(1)); await page.locator("#evaluation-start").click(); await waitForRows(page, 1);
  const report = await exportReport(page); const row = report.results[0];
  expect(row.matched_rank).toBe(7); expect(report.summary.mean_reciprocal_rank).toBeCloseTo(1 / 7, 6);
  expect(row.evidence).toHaveLength(6); expect(row.evidence.at(-1)).toMatchObject({ rank: 7, matched: true, truncated: true });
  expect(row.evidence.every((item) => item.text.length <= 2001)).toBe(true);
  await page.locator("#evaluation-results details > summary").first().click();
  await expect(page.locator("#evaluation-results")).toContainText("ORBIT-42");
});

test("test labels and retrieved evidence remain inert text", async ({ page }) => {
  const fixture = await workspace(page);
  const hostile = '<img src=x onerror="window.evaluationXss=1">';
  fixture.respond = () => response([hit("manual.pdf#page=2", `ORBIT-42 ${hostile}`)]);
  await loadDataset(page, [{ question: hostile, expected_source: "manual.pdf#page=2" }]);
  await page.locator("#evaluation-start").click(); await waitForRows(page, 1);
  await page.locator("#evaluation-results details > summary").first().click();
  await expect(page.locator("#evaluation-results")).toContainText(hostile);
  await expect(page.locator("#evaluation-results img, #evaluation-results script")).toHaveCount(0);
  expect(await page.evaluate(() => window.evaluationXss)).toBeUndefined();
});

test("filter changes discard an active response and prevent later questions from running", async ({ page }) => {
  const fixture = await workspace(page); const filter = await addFilter(page, "Guide"); const gate = deferred(); let entered = false;
  await loadDataset(page, questions(3));
  await page.route("**/retrieve", async (route) => { entered = true; await gate.promise; await reply(route, response([hit("manual.pdf#page=2", "Late old filter result")])).catch(() => {}); });
  await page.locator("#evaluation-start").click(); await expect.poll(() => entered).toBe(true);
  await filter.locator("[data-filter-value]").fill("Reference"); gate.resolve();
  await expect(page.locator("#evaluation-pause")).toBeDisabled();
  await expect(page.locator("#evaluation-results")).not.toContainText("Late old filter result");
  expect(retrievals(fixture)).toHaveLength(1);
  await page.unroute("**/retrieve");
  await page.locator("#evaluation-start").click(); await waitForRows(page, 3);
  expect(retrievals(fixture).slice(1).every((call) => call.body.document_filters[0].value === "Reference")).toBe(true);
});

test("collection changes clear reports and late responses cannot populate the new collection", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await loadDataset(page, questions(2));
  await page.route("**/collections/knowledge/retrieve", async (route) => { entered = true; await gate.promise; await reply(route, response([hit("manual.pdf#page=2", "Private old collection result")])).catch(() => {}); });
  await page.locator("#evaluation-start").click(); await expect.poll(() => entered).toBe(true);
  await page.locator("#graph-collection").selectOption("archive"); gate.resolve();
  await expect(page.locator("#evaluation-results [data-evaluation-row]")).toHaveCount(0);
  await expect(page.locator("#evaluation-export")).toBeDisabled();
  await loadDataset(page, questions(1)); await page.locator("#evaluation-start").click(); await waitForRows(page, 1);
  const report = await exportReport(page); expect(report.collection).toBe("archive");
  expect(retrievals(fixture)).toHaveLength(2); expect(retrievals(fixture)[1].path).toBe("/v1/graph/collections/archive/retrieve");
  expect(JSON.stringify(report)).not.toContain("Private old collection result");
});

test("changing the API token clears the test set and ignores a late unauthorized evaluation", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await loadDataset(page, [{ question: "Private evaluation question", expected_source: "private-source" }], "private-set.json");
  await page.route("**/retrieve", async (route) => { entered = true; await gate.promise; await reply(route, { error: { code: "unauthorized", message: "Old token error" } }, 401).catch(() => {}); });
  await page.locator("#evaluation-start").click(); await expect.poll(() => entered).toBe(true);
  await page.locator("#open-token").click(); await page.locator("#token-input").fill("replacement-evaluation-session"); await page.locator("#save-token").click(); gate.resolve();
  await expect(page.locator("#status-label")).toHaveText("Connected");
  await expect(page.locator("#evaluation-start")).toBeDisabled(); await expect(page.locator("#evaluation-export")).toBeDisabled();
  await expect(page.locator("#evaluation-results [data-evaluation-row]")).toHaveCount(0);
  await expect(page.locator("#evaluation-dataset")).not.toContainText("private-set");
  await expect(page.locator("#evaluation-status")).not.toContainText("Old token error");
  await expect(page.locator("#token-dialog")).not.toBeVisible();
  expect(await page.evaluate(() => JSON.stringify({ ...localStorage, ...sessionStorage }))).not.toContain("Private evaluation");
  expect(retrievals(fixture)).toHaveLength(1);
});

test("creating a collection cancels evaluation and the next run uses only the new collection", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await loadDataset(page, questions(2));
  await page.route("**/collections/knowledge/retrieve", async (route) => {
    entered = true; await gate.promise;
    await reply(route, response([hit("manual.pdf#page=2", "Private old collection evidence")])).catch(() => {});
  });
  await page.locator("#evaluation-start").click(); await expect.poll(() => entered).toBe(true);
  await page.locator("#graph-create-open").click();
  await page.locator("#graph-create-name").fill("new_sources");
  await page.locator("#graph-create-submit").click();
  await expect(page.locator("#graph-create-dialog")).not.toBeVisible();
  await expect(page.locator("#graph-collection")).toHaveValue("new_sources");
  gate.resolve(); await openEvaluation(page);
  await expect(page.locator("#evaluation-results [data-evaluation-row]")).toHaveCount(0);
  await expect(page.locator("#evaluation-export")).toBeDisabled();
  await loadDataset(page, questions(1)); await page.locator("#evaluation-start").click(); await waitForRows(page, 1);
  const report = await exportReport(page);
  expect(report.collection).toBe("new_sources");
  expect(JSON.stringify(report)).not.toContain("Private old collection evidence");
  expect(retrievals(fixture)).toHaveLength(2);
  expect(retrievals(fixture)[1].path).toBe("/v1/graph/collections/new_sources/retrieve");
});

test("large completed evaluations render bounded pages and export every row", async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 1512, height: 1050 });
  const fixture = await workspace(page); await loadDataset(page, questions(55)); await page.locator("#evaluation-limit").fill("55");
  await page.locator("#evaluation-start").click(); await waitForRows(page, 20);
  expect(retrievals(fixture)).toHaveLength(55); expect((await exportReport(page)).results).toHaveLength(55);
  await expect(page.locator("#evaluation-range")).toContainText(/1.?20.*55/);
  await page.locator("#evaluation-next").click(); await expect(page.locator("#evaluation-results [data-evaluation-row]")).toHaveCount(20);
  await page.locator("#evaluation-next").click(); await expect(page.locator("#evaluation-results [data-evaluation-row]")).toHaveCount(15);
  await expect(page.locator("#evaluation-next")).toBeDisabled();
  await page.locator("#evaluation-previous").click(); await page.locator("#evaluation-previous").click();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  // Tall component captures otherwise include the sticky page header midway.
  await page.locator(".topbar").evaluate((element) => { element.style.visibility = "hidden"; });
  await page.locator("#playground-evaluation").screenshot({ path: testInfo.outputPath("evaluation-desktop.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  if (await page.locator("#playground-settings").isVisible()) await page.locator("#playground-settings-close").click();
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.locator("#playground-evaluation").screenshot({ path: testInfo.outputPath("evaluation-mobile.png"), style: ".topbar { visibility: hidden; }" });
});
