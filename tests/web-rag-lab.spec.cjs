"use strict";
const { test, expect } = require("@playwright/test");
const path = require("node:path");
const fs = require("node:fs/promises");
const reply = (route, body, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
const deferred = () => { let resolve; const promise = new Promise((done) => { resolve = done; }); return { promise, resolve }; };
// A real, deliberately small PDF. Extraction runs through the bundled PDF.js worker.
function pdf(pages = ["First page: model P00001 uses sensor Orion.", "Second page: Orion needs threshold 17."]) {
  const objects = ["<< /Type /Catalog /Pages 2 0 R >>", `<< /Type /Pages /Kids [${pages.map((_, i) => `${4 + i * 2} 0 R`).join(" ")}] /Count ${pages.length} >>`, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"];
  for (const text of pages) {
    const stream = `BT /F1 12 Tf 50 700 Td (${text.replace(/[\\()]/g, "\\$&")}) Tj ET`;
    objects.push(`<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /Font << /F1 3 0 R >> >> /Contents ${objects.length + 2} 0 R >>`, `<< /Length ${Buffer.byteLength(stream)} >>\nstream\n${stream}\nendstream`);
  }
  let result = "%PDF-1.7\n"; const offsets = [0];
  objects.forEach((object, i) => { offsets.push(Buffer.byteLength(result)); result += `${i + 1} 0 obj\n${object}\nendobj\n`; });
  const start = Buffer.byteLength(result);
  result += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n${offsets.slice(1).map((offset) => `${String(offset).padStart(10, "0")} 00000 n \n`).join("")}trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${start}\n%%EOF\n`;
  return Buffer.from(result);
}
const file = (name = "manual.pdf", pages) => ({ name, mimeType: "application/pdf", buffer: pdf(pages) });
const profile = { provider: "openai", model: "text-embedding-3-small", dimensions: 3, context_format_version: 1 };
const collection = (name) => ({ config: { name, profile, semantic_neighbors: 0, semantic_threshold: .8 }, document_columns: [], revision: 7, document_count: 0, chunk_count: 0, edge_count: 0, tables: {} });
const hit = { chunk_id: "chunk-two", document_id: "pdf-page-two", title: "Manual page 2", source: "manual.pdf#page=2", text: "Orion requires threshold 17.", ordinal: 0, start_byte: 0, end_byte: 28, seed: false, depth: 1, similarity: .82, lexical_score: .4, fusion_score: .028, selection_score: .7, retrieval_path: { seed_chunk_id: "chunk-one", edges: [{ from_chunk: "chunk-one", to_chunk: "chunk-two", kind: "references", weight: .9 }] } };
const chatResult = (answer = "Orion requires threshold 17 [S1].") => ({ answer, answer_status: answer === null ? "retrieval_only" : "answered", citation_status: answer === null ? "not_applicable" : "valid_labels", cited_labels: answer === null ? [] : ["S1"], evidence: answer === null ? [] : [{ label: "S1", quote: hit.text }], speech_text: null, retrieval: { collection: "knowledge", revision: 7, hits: [hit], edges: [], candidate_count: 12, context_bytes: 28, lexical_cache_hit: true, truncated: false, timings: { embedding_ms: 12.5, search_ms: 2, selection_ms: .5, total_ms: 15 }, traversal_seed_ids: ["chunk-one"], reranking: { method: "local", model: null } }, generation: { provider: answer ? "openai" : null, model: answer ? "gpt-4.1-mini" : null, input_tokens: answer ? 42 : 0, output_tokens: answer ? 10 : 0 }, timings: { embedding_ms: 12.5, search_ms: 2, reranking_ms: 0, selection_ms: .5, generation_ms: answer ? 20 : 0, total_ms: answer ? 35 : 15 }, citations: [{ label: "S1", chunk_id: hit.chunk_id, document_id: hit.document_id, title: hit.title, source: hit.source, start_byte: 0, end_byte: 28 }], warnings: [] });
const groundedResult = (overrides = {}) => ({ ...chatResult(), answer_status: "answered", citation_status: "valid_labels", cited_labels: ["S1"], speech_text: null, evidence: [{ label: "S1", quote: hit.text }], retrieval_query: "What threshold does Orion need?", query_context: { mode: "question", rewritten: false, duration_ms: 0, generation: { provider: null, model: null, input_tokens: 0, output_tokens: 0 } }, ...overrides });
async function workspace(page, { generationConfigured = true, embeddingConfigured = true, dimensions = 3, documents = true } = {}) {
  const fixture = { calls: [], documents: new Map(), revision: 7, collections: [collection("knowledge"), collection("archive")], origins: [] };
  page.on("request", (request) => { fixture.origins.push(new URL(request.url()).origin); if (new URL(request.url()).pathname.startsWith("/v1/")) fixture.calls.push({ path: new URL(request.url()).pathname, method: request.method(), body: request.postData() ? request.postDataJSON() : null }); });
  await page.route("**/healthz", (route) => reply(route, { status: "ok", version: "0.10.0", storage: "memory" }));
  await page.route("**/v1/**", (route) => {
    const pathname = new URL(route.request().url()).pathname; const body = route.request().postData() ? route.request().postDataJSON() : null;
    if (pathname === "/v1/tables") return reply(route, { revision: fixture.revision, tables: [] });
    if (pathname === "/v1/settings/embeddings") return reply(route, { ...profile, dimensions, configured: embeddingConfigured, generation_configured: generationConfigured, persistence: "memory", timeout_seconds: 60, batch_size: 32, max_concurrent_requests: 4, providers: [{ id: "openai", label: "OpenAI", configured: true, models: [{ id: profile.model, dimensions: [3], default_dimensions: 3 }] }] });
    if (pathname === "/v1/settings/reranking") return reply(route, { configured: false, model: "rerank-2.5", timeout_seconds: 60, models: [] });
    if (pathname === "/v1/graph/collections") {
      if (route.request().method() === "POST") { const added = collection(body.name); fixture.collections.push(added); return reply(route, added); }
      return reply(route, fixture.collections);
    }
    if (pathname.endsWith("/capacity")) return reply(route, { collection: pathname.split("/")[4], revision: fixture.revision, usage: { chunks: fixture.documents.size, edges: 0, vector_elements: fixture.documents.size * 3, text_bytes: 100 }, limits: { chunks: 10000, edges: 340000, vector_elements: 33554432, text_bytes: 67108864, document_bytes: 1048576, document_chunks: 256 } });
    if (pathname.endsWith("/graph")) return reply(route, { collection: pathname.split("/")[4], revision: fixture.revision, nodes: [], edges: [], total_nodes: 0, total_edges: 0, offset: 0, limit: 100, truncated: false });
    if (pathname === "/v1/graph/chunk") return reply(route, { chunks: [{ ordinal: 0, text: body.text, embedding_text: body.text, byte_start: 0, byte_end: Buffer.byteLength(body.text) }], embedding_bytes: Buffer.byteLength(body.text), context_format_version: 1 });
    if (pathname.endsWith("/documents")) { const unchanged = fixture.documents.has(body.id); fixture.documents.set(body.id, body); fixture.revision += 1; return reply(route, { document_id: body.id, revision: fixture.revision, chunks: 1, edges_created: 0, unchanged }); }
    if (pathname.endsWith("/chat")) { const result = chatResult(body.mode === "retrieve" ? null : undefined); result.retrieval.collection = pathname.split("/")[4]; return reply(route, result); }
    if (pathname.endsWith("/neighborhood")) return reply(route, { collection: "knowledge", revision: 7, root_chunk: "chunk-one", nodes: [{ ...hit, chunk_id: "chunk-one", depth: 0 }], edges: [], truncated: false });
    return reply(route, { error: { code: "not_found", message: "Unexpected test request" } }, 404);
  });
  await page.goto("/"); await expect(page.locator("#status-label")).toHaveText("Connected"); await expect(page.locator("#view-title")).toHaveText("Playground"); await expect(page.locator("#graph-collection")).toHaveValue("knowledge");
  if (documents) await page.locator("#graph-upload-open").click();
  return fixture;
}
const saves = (fixture) => fixture.calls.filter((call) => call.path.endsWith("/documents") && call.method === "POST");
const chats = (fixture) => fixture.calls.filter((call) => call.path.endsWith("/chat"));
async function ask(page, text = "What threshold does Orion need?") { await page.locator('[data-playground-tab="chat"]').click(); await page.locator("#graph-chat-question").fill(text); await page.locator("#graph-chat-submit").click(); }

test("real PDF pages extract offline, preview before saving, and retain deterministic page citations", async ({ page }) => {
  const fixture = await workspace(page); await page.locator("#graph-pdf-files").setInputFiles(file());
  await expect(page.locator("#graph-upload-summary")).toContainText("1 queued"); expect(saves(fixture)).toHaveLength(0);
  await page.locator("#graph-upload-start").click(); await expect(page.locator("#graph-upload-summary")).toContainText("1 complete");
  const documents = saves(fixture); expect(documents).toHaveLength(2); expect(documents[0].body.text).toContain("sensor Orion"); expect(documents[1].body.text).toContain("threshold 17");
  expect(documents.map((call) => call.body.source)).toEqual(["manual.pdf#page=1", "manual.pdf#page=2"]);
  expect(documents[0].body).toMatchObject({ metadata: { filename: "manual.pdf", page: 1, part: 1 }, chunking: { max_characters: 800, overlap_characters: 100, max_chunks: 256 }, expected_revision: 7 });
  expect(documents[0].body.id).toMatch(/^pdf_[a-f0-9]{64}$/); expect(documents[1].body.expected_revision).toBe(8);
  expect(fixture.calls.filter((call) => call.path === "/v1/graph/chunk")).toHaveLength(2);
  await page.locator("#graph-pdf-files").setInputFiles(file()); await page.locator("#graph-upload-start").click(); await expect(page.locator("#graph-upload-summary")).toContainText("2 complete");
  expect(saves(fixture).slice(2).map((call) => call.body.id)).toEqual(documents.map((call) => call.body.id));
  expect(new Set(fixture.origins)).toEqual(new Set([new URL(page.url()).origin])); await expect(page.locator("#graph-capacity")).toContainText("10,000");
});

test("pause finishes the active save and resume skips committed page parts", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await page.route("**/collections/knowledge/documents", async (route) => { if (!entered) { entered = true; await gate.promise; } await route.fallback(); });
  await page.locator("#graph-pdf-files").setInputFiles(file()); await page.locator("#graph-upload-start").click(); await expect.poll(() => entered).toBe(true);
  await page.locator("#graph-upload-pause").click(); gate.resolve(); await expect(page.locator("#graph-upload-start")).toHaveText("Resume import"); await expect(page.locator("#graph-upload-start")).toBeEnabled(); expect(saves(fixture)).toHaveLength(1);
  await page.locator("#graph-upload-start").click(); await expect(page.locator("#graph-upload-summary")).toContainText("1 complete"); expect(saves(fixture)).toHaveLength(2);
});

test("failed saves pause without retry and explicit retry skips already completed parts", async ({ page }) => {
  const fixture = await workspace(page); let attempt = 0;
  await page.route("**/collections/knowledge/documents", async (route) => { attempt += 1; if (attempt === 2) return reply(route, { error: { code: "stale_revision", message: "Collection changed before commit" } }, 409); await route.fallback(); });
  await page.locator("#graph-pdf-files").setInputFiles(file()); await page.locator("#graph-upload-start").click(); await expect(page.locator("#graph-upload-summary")).toContainText("1 need attention");
  await expect(page.locator("#graph-upload-status")).toContainText("No automatic retry"); expect(saves(fixture)).toHaveLength(2);
  await page.locator("#graph-upload-retry").click(); await expect(page.locator("#graph-upload-summary")).toContainText("1 complete"); expect(saves(fixture)).toHaveLength(3); expect(saves(fixture)[2].body.id).toBe(saves(fixture)[1].body.id);
});

test("protected, malformed and scanned PDFs finish with explicit outcomes and no provider calls", async ({ page }) => {
  const fixture = await workspace(page); await page.locator("#graph-pdf-files").setInputFiles([{ name: "protected.pdf", mimeType: "application/pdf", buffer: await fs.readFile(path.join(__dirname, "fixtures/rag-protected.pdf")) }, { name: "broken.pdf", mimeType: "application/pdf", buffer: Buffer.from("invalid pdf") }, file("scanned.pdf", [""])]);
  await page.locator("#graph-upload-start").click(); await expect(page.locator("#graph-upload-summary")).toContainText("0 complete · 3 need attention");
  await expect(page.locator("#graph-upload-list")).toContainText("Password-protected PDF"); await expect(page.locator("#graph-upload-list")).toContainText("run OCR"); expect(saves(fixture)).toHaveLength(0);
});

test("collection changes preserve an uncertain file and late saves cannot update the new collection", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await page.route("**/collections/knowledge/documents", async (route) => { entered = true; await gate.promise; await reply(route, { document_id: "old", revision: 999, chunks: 1 }).catch(() => {}); });
  await page.locator("#graph-pdf-files").setInputFiles(file()); await page.locator("#graph-upload-start").click(); await expect.poll(() => entered).toBe(true);
  await page.locator("#graph-collection").selectOption("archive"); gate.resolve(); await expect(page.locator("#graph-upload-summary")).toHaveText("No files queued.");
  await page.locator("#graph-collection").selectOption("knowledge"); await expect(page.locator("#graph-upload-list")).toContainText("uncertain"); expect(saves(fixture)).toHaveLength(1); await expect(page.locator("#graph-upload-list")).toContainText("may still finish");
});

test("chat renders safe citations, ranking, graph paths, timing and diagnostic downloads", async ({ page }) => {
  const fixture = await workspace(page); await ask(page); await expect(page.locator(".graph-chat-answer")).toContainText("threshold 17");
  expect(chats(fixture)[0].body).toMatchObject({ text: "What threshold does Orion need?", history: [], model: "gpt-4.1-mini", mode: "answer", retrieval: { candidate_limit: 40, seed_limit: 12, max_context_bytes: 24000, reranker: "local" } });
  expect(chats(fixture)[0].body.retrieval).not.toHaveProperty("text");
  await page.getByRole("button", { name: "Inspect source S1" }).click(); await expect(page.locator(".graph-chat-evidence")).toHaveAttribute("open", "");
  await expect(page.locator(".graph-chat-timings")).toContainText("Embedding: 12.5 ms"); await expect(page.locator(".graph-chat-seeds")).toContainText("chunk-one"); await expect(page.locator(".graph-chat-sources")).toContainText("manual.pdf#page=2");
  await page.locator(".graph-chat-sources summary").click(); await expect(page.locator(".graph-path-steps")).toContainText("references");
  const download = page.waitForEvent("download"); await page.getByRole("button", { name: "Download diagnostics" }).click(); expect((await download).suggestedFilename()).toBe("vectors-rag-turn-1.json");
  await ask(page, "Why?"); await expect(page.locator(".graph-chat-turn")).toHaveCount(2); expect(chats(fixture)[1].body.history).toHaveLength(2);
});

test("retrieval-only chat works without an answer key; unsupported context stops before requests", async ({ page }) => {
  const fixture = await workspace(page, { generationConfigured: false }); await ask(page); await expect(page.locator("#graph-chat-status")).toContainText("OpenAI API key"); expect(chats(fixture)).toHaveLength(0);
  await page.locator("#graph-chat-mode").selectOption("retrieve"); await ask(page); await expect(page.locator(".graph-chat-answer")).toContainText(/no answer model/i); expect(chats(fixture)[0].body.mode).toBe("retrieve");
  if (await page.locator("#playground-settings").isHidden()) await page.locator("#playground-settings-toggle").click(); await page.locator("#graph-search-form summary").filter({ hasText: "Context and diversity" }).click(); await page.locator("#graph-context-budget").fill("70000"); await ask(page); await expect(page.locator("#graph-chat-status")).toContainText("65,536 bytes"); expect(chats(fixture)).toHaveLength(1);
});

test("stopping chat or changing collection discards late answers and does not add history", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await page.route("**/chat", async (route) => { entered = true; await gate.promise; await reply(route, chatResult("Old private answer [S1]")).catch(() => {}); });
  await ask(page); await expect.poll(() => entered).toBe(true); await page.locator("#graph-chat-stop").click(); gate.resolve(); await expect(page.locator("#graph-chat-status")).toContainText("Stopped waiting"); await expect(page.locator(".graph-chat-turn")).toHaveCount(0);
  await page.unroute("**/chat"); await ask(page, "Fresh question"); await expect(page.locator(".graph-chat-turn")).toHaveCount(1); expect(chats(fixture)[1].body.history).toEqual([]);
  await page.locator("#graph-collection").selectOption("archive"); await expect(page.locator(".graph-chat-turn")).toHaveCount(0);
});

test("provider text remains inert and new panels fit mobile and desktop", async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 1512, height: 1050 });
  await workspace(page, { documents: false }); await page.locator("#graph-chat-grounding").selectOption("standard"); await page.route("**/chat", (route) => reply(route, chatResult('<img src=x onerror="window.chatXss=1"> [S1] [S99]')));
  await ask(page); await expect(page.locator(".graph-chat-answer img")).toHaveCount(0); expect(await page.evaluate(() => window.chatXss)).toBeUndefined(); await expect(page.locator(".graph-chat-answer .graph-citation-button")).toHaveCount(1);
  await page.unroute("**/chat"); await page.locator("#graph-chat-clear").click(); await ask(page); await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  await page.locator(".graph-chat-evidence > summary").click(); await page.locator(".graph-chat-sources summary").click();
  await page.locator(".topbar").evaluate((element) => { element.style.visibility = "hidden"; });
  await page.locator("#graph-chat-panel").screenshot({ path: testInfo.outputPath("rag-chat-desktop.png") });
  await page.setViewportSize({ width: 390, height: 844 }); expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.locator("#graph-chat-panel").screenshot({ path: testInfo.outputPath("rag-chat-mobile.png"), style: ".topbar { visibility: hidden; }" });
});

test("PDF splitting preserves Unicode and folder paths distinguish documents with identical names", async ({ page }) => {
  await workspace(page);
  const result = await page.evaluate(async () => { const module = await import("/assets/pdf-import.js"); const text = "🧭Zażółć".repeat(12000); const parts = module.splitPdfText(text); const bytes = new TextEncoder();
    const files = [new File(["same"], "guide.pdf"), new File(["same"], "guide.pdf")]; Object.defineProperty(files[0], "webkitRelativePath", { value: "a/guide.pdf" }); Object.defineProperty(files[1], "webkitRelativePath", { value: "b/guide.pdf" });
    return { intact: parts.join("") === text, bounded: parts.every((part) => bytes.encode(part).length <= 65536), names: files.map(module.pdfRelativePath), a: await module.pdfDocumentId("a/content", 1, 1), b: await module.pdfDocumentId("b/content", 1, 1) }; });
  expect(result.intact).toBe(true); expect(result.bounded).toBe(true); expect(result.names).toEqual(["a/guide.pdf", "b/guide.pdf"]); expect(result.a).not.toBe(result.b);
});

test("folder picker preserves nested paths and same-named PDFs have distinct identities", async ({ page }, testInfo) => {
  const fixture = await workspace(page); const folder = testInfo.outputPath("library");
  for (const name of ["a", "b"]) { await fs.mkdir(path.join(folder, name), { recursive: true }); await fs.writeFile(path.join(folder, name, "guide.pdf"), pdf(["Same content in a different folder."])); }
  await page.locator("#graph-pdf-folder").setInputFiles(folder); await expect(page.locator("#graph-upload-summary")).toContainText("2 queued"); await page.locator("#graph-upload-start").click();
  await expect(page.locator("#graph-upload-summary")).toContainText("2 complete"); const documents = saves(fixture); expect(documents).toHaveLength(2);
  expect(new Set(documents.map((call) => call.body.id)).size).toBe(2); expect(documents.map((call) => call.body.source).sort()).toEqual(["library/a/guide.pdf#page=1", "library/b/guide.pdf#page=1"]);
});

test("large queues render bounded pages and size checks reject oversized PDFs", async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 1512, height: 1050 });
  const fixture = await workspace(page); await page.locator("#graph-pdf-files").setInputFiles(Array.from({ length: 55 }, (_, i) => file(`manual-${i + 1}.pdf`, ["Text"])));
  await expect(page.locator(".graph-upload-item")).toHaveCount(50); await expect(page.locator("#graph-upload-range")).toHaveText("1–50 of 55 files"); await page.locator("#graph-upload-next").click(); await expect(page.locator(".graph-upload-item")).toHaveCount(5); expect(saves(fixture)).toHaveLength(0);
  const error = await page.evaluate(async () => { const { validatePdfFile } = await import("/assets/pdf-import.js"); try { validatePdfFile({ name: "huge.pdf", size: 50 * 1024 * 1024 + 1 }); } catch (error) { return error.message; } }); expect(error).toContain("50 MiB");
  await page.locator(".topbar").evaluate((element) => { element.style.visibility = "hidden"; });
  await page.locator("#graph-upload-panel").screenshot({ path: testInfo.outputPath("pdf-queue-desktop.png"), style: ".topbar { visibility: hidden; }" });
  await page.setViewportSize({ width: 390, height: 844 }); expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true); await page.locator("#graph-upload-panel").screenshot({ path: testInfo.outputPath("pdf-queue-mobile.png"), style: ".topbar { visibility: hidden; }" });
});

test("token changes clear selected files, chat history and late provider responses", async ({ page }) => {
  await workspace(page); const writeGate = deferred(); const chatGate = deferred(); let writing = false; let chatting = false;
  await page.route("**/collections/knowledge/documents", async (route) => { writing = true; await writeGate.promise; await reply(route, { document_id: "old", revision: 999, chunks: 1 }).catch(() => {}); });
  await page.route("**/chat", async (route) => { chatting = true; await chatGate.promise; await reply(route, chatResult("Private old answer [S1]")).catch(() => {}); });
  await page.locator("#graph-pdf-files").setInputFiles(file()); await page.locator("#graph-upload-start").click(); await expect.poll(() => writing).toBe(true); await ask(page); await expect.poll(() => chatting).toBe(true);
  await page.locator("#open-token").click(); await page.locator("#token-input").fill("replacement-session"); await page.locator("#save-token").click(); writeGate.resolve(); chatGate.resolve();
  await expect(page.locator("#graph-upload-summary")).toHaveText("No files queued."); await expect(page.locator(".graph-chat-turn")).toHaveCount(0); await expect(page.locator("#graph-chat-question")).toHaveValue(""); await expect(page.locator("#graph-pdf-files")).toHaveValue(""); await expect(page.locator("#graph-status")).toContainText("may still finish");
});

test("Playground opens to chat and its tabs preserve drafts without running providers", async ({ page }) => {
  const fixture = await workspace(page, { documents: false });
  await expect(page.locator('[data-playground-tab="chat"]')).toHaveAttribute("aria-selected", "true");
  await expect(page.locator("#playground-chat")).toBeVisible();
  await expect(page.locator("#playground-documents")).toBeHidden();
  await expect(page.locator("#playground-graph")).toBeHidden();
  await expect(page.locator(".graph-chat-evidence")).toHaveCount(0);
  await page.locator("#graph-chat-question").fill("A question to keep while browsing");
  await page.locator('[data-playground-tab="chat"]').focus(); await page.keyboard.press("ArrowRight");
  await expect(page.locator('[data-playground-tab="documents"]')).toBeFocused();
  await expect(page.locator("#playground-documents")).toBeVisible();
  await expect(page.locator("#playground-chat")).toBeHidden();
  await page.keyboard.press("End");
  await expect(page.locator('[data-playground-tab="graph"]')).toBeFocused();
  await expect(page.locator("#playground-graph")).toBeVisible();
  await expect(page.locator("#playground-documents")).toBeHidden();
  await page.keyboard.press("Home");
  await expect(page.locator("#graph-chat-question")).toHaveValue("A question to keep while browsing");
  expect(fixture.calls.filter((call) => call.method === "POST")).toHaveLength(0);
});

test("Playground keeps its conversation and optional settings usable on desktop and mobile", async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 1512, height: 1050 });
  await workspace(page, { documents: false });
  await expect(page.locator("#playground-settings")).toBeVisible();
  await expect(page.locator("#graph-question")).toBeHidden();
  await expect(page.locator("#graph-context-budget")).toBeHidden();
  await page.screenshot({ path: testInfo.outputPath("playground-empty-desktop.png"), fullPage: true });
  await ask(page); await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  await expect(page.locator(".graph-chat-evidence")).not.toHaveAttribute("open", "");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({ path: testInfo.outputPath("playground-answer-desktop.png"), fullPage: true });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.reload(); await expect(page.locator("#status-label")).toHaveText("Connected");
  await expect(page.locator("#playground-settings")).toBeHidden();
  await expect(page.locator("#playground-settings-toggle")).toHaveAttribute("aria-expanded", "false");
  await ask(page); await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  await page.screenshot({ path: testInfo.outputPath("playground-answer-mobile.png"), fullPage: true });
  if (await page.locator("#playground-settings").isHidden()) await page.locator("#playground-settings-toggle").click();
  await expect(page.locator("#playground-settings")).toBeVisible();
  await page.locator("#playground-strategy").selectOption("keyword");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({ path: testInfo.outputPath("playground-settings-mobile.png"), fullPage: true });
  await page.locator("#playground-settings-close").click();
  await expect(page.locator("#playground-settings")).toBeHidden();
  await expect(page.locator("#graph-chat-question")).toBeVisible();
});

for (const [strategy, weights] of [
  ["graph", { max_hops: 1, vector_weight: 1, lexical_weight: 1 }],
  ["hybrid", { max_hops: 0, vector_weight: 1, lexical_weight: 1 }],
  ["vector", { max_hops: 0, vector_weight: 1, lexical_weight: 0 }],
  ["keyword", { max_hops: 0, vector_weight: 0, lexical_weight: 1 }],
]) {
  test(`Playground ${strategy} strategy sends the selected retrieval plan`, async ({ page }) => {
    const fixture = await workspace(page, { documents: false });
    await page.locator("#playground-strategy").selectOption(strategy);
    await page.locator("#graph-chat-mode").selectOption("retrieve");
    await ask(page);
    await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
    expect(chats(fixture)).toHaveLength(1);
    expect(chats(fixture)[0].body).toMatchObject({ mode: "retrieve", history: [], retrieval: weights });
    await expect(page.locator(".graph-chat-evidence")).not.toHaveAttribute("open", "");
    expect(fixture.calls.filter((call) => call.path.endsWith("/retrieve"))).toHaveLength(0);
  });
}

test("keyword retrieval can run with no embedding key and a different active profile", async ({ page }) => {
  const fixture = await workspace(page, { documents: false, generationConfigured: false, embeddingConfigured: false, dimensions: 2 });
  await page.locator("#playground-strategy").selectOption("keyword");
  await page.locator("#graph-chat-mode").selectOption("retrieve");
  await ask(page);
  await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  expect(chats(fixture)[0].body.retrieval).toMatchObject({ vector_weight: 0, lexical_weight: 1, max_hops: 0 });
  await page.locator("#playground-strategy").selectOption("vector");
  await ask(page);
  await expect(page.locator("#graph-chat-status")).toContainText(/key|profile/i);
  expect(chats(fixture)).toHaveLength(1);
});

test("run again uses the last question with current settings and compares matching runs", async ({ page }) => {
  const fixture = await workspace(page, { documents: false });
  await expect(page.locator("#playground-run-again")).toBeDisabled();
  await expect(page.locator("#playground-compare")).toBeDisabled();
  await ask(page, "What threshold does Orion need?");
  await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  await expect(page.locator("#playground-run-again")).toBeEnabled();
  await page.locator("#playground-strategy").selectOption("keyword");
  await page.locator("#playground-run-again").click();
  await expect(page.locator(".graph-chat-turn")).toHaveCount(2);
  expect(chats(fixture)).toHaveLength(2);
  expect(chats(fixture)[1].body).toMatchObject({ text: chats(fixture)[0].body.text, history: [], retrieval: { vector_weight: 0, lexical_weight: 1, max_hops: 0 } });
  await expect(page.locator("#playground-comparison")).toBeVisible();
  await expect(page.locator(".playground-comparison-overlap")).toHaveText("1 shared · 0 added · 0 removed passages");
  await expect(page.locator("#playground-comparison")).toContainText("Retrieval 15.0 ms");
  await page.locator("#playground-compare").click();
  await expect(page.locator("#playground-comparison")).toBeHidden();
  await page.locator("#playground-compare").click();
  await expect(page.locator("#playground-comparison")).toBeVisible();
  expect(chats(fixture)).toHaveLength(2);
  await ask(page, "A different question");
  await expect(page.locator(".graph-chat-turn")).toHaveCount(3);
  await expect(page.locator("#playground-compare")).toBeDisabled();
});

test("comparison warns when the collection changed and reset clears repeat state", async ({ page }) => {
  const fixture = await workspace(page, { documents: false });
  await ask(page); await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  await page.route("**/chat", (route) => { const result = chatResult(); result.retrieval.revision = 8; return reply(route, result); });
  await page.locator("#playground-run-again").click(); await expect(page.locator(".graph-chat-turn")).toHaveCount(2);
  await expect(page.locator("#playground-comparison")).toContainText(/revision|collection changed/i);
  await page.locator("#graph-chat-clear").click();
  await expect(page.locator(".graph-chat-turn")).toHaveCount(0);
  await expect(page.locator("#playground-run-again")).toBeDisabled();
  await expect(page.locator("#playground-compare")).toBeDisabled();
  await expect(page.locator("#playground-comparison")).toBeHidden();
  expect(chats(fixture)).toHaveLength(2);
});

test("editing filters preserves past runs but isolates history and comparisons", async ({ page }) => {
  const fixture = await workspace(page, { documents: false });
  await ask(page); await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  await page.locator("#playground-run-again").click(); await expect(page.locator(".graph-chat-turn")).toHaveCount(2);
  await expect(page.locator("#playground-comparison")).toBeVisible();
  if (await page.locator("#playground-settings").isHidden()) await page.locator("#playground-settings-toggle").click();
  await page.locator("#graph-filters-panel > summary").click();
  await page.locator("#graph-filter-add").click();
  const row = page.locator("[data-graph-filter-row]").last();
  await row.locator("[data-filter-column]").selectOption("title");
  await row.locator("[data-filter-value]").fill("Manual page 2");
  await expect(page.locator(".graph-chat-turn")).toHaveCount(2);
  await expect(page.locator("#playground-comparison")).toBeHidden();
  await expect(page.locator("#playground-run-again")).toBeEnabled();
  expect(chats(fixture)).toHaveLength(2);
  await ask(page); await expect(page.locator(".graph-chat-turn")).toHaveCount(3);
  expect(chats(fixture)[2].body).toMatchObject({ history: [], retrieval: { document_filters: [{ column: "title", operator: "eq", value: "Manual page 2" }] } });
  await expect(page.locator("#playground-compare")).toBeDisabled();
});

test("creating a collection clears previous and pending chat before its first question", async ({ page }) => {
  const fixture = await workspace(page, { documents: false });
  await ask(page, "First collection question"); await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  const gate = deferred(); let entered = false;
  await page.route("**/collections/knowledge/chat", async (route) => {
    entered = true; await gate.promise; await reply(route, chatResult("Private answer from the old collection [S1]")).catch(() => {});
  });
  await ask(page, "Pending old collection question"); await expect.poll(() => entered).toBe(true);
  await page.locator("#graph-create-open").click();
  await page.locator("#graph-create-name").fill("new_sources");
  await page.locator("#graph-create-submit").click();
  await expect(page.locator("#graph-create-dialog")).not.toBeVisible();
  await expect(page.locator("#graph-collection")).toHaveValue("new_sources");
  await expect(page.locator("#playground-documents")).toBeVisible();
  await expect(page.locator(".graph-chat-turn")).toHaveCount(0);
  await expect(page.locator("#graph-chat-submit")).toBeEnabled();
  await expect(page.locator("#playground-run-again")).toBeDisabled();
  gate.resolve();
  await ask(page, "Question for the new collection");
  await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  await expect(page.locator("#graph-chat-turns")).not.toContainText("Private answer from the old collection");
  expect(chats(fixture).at(-1)).toMatchObject({ path: "/v1/graph/collections/new_sources/chat", body: { text: "Question for the new collection", history: [] } });
  expect(chats(fixture)).toHaveLength(3);
  await expect(page.locator("#graph-chat-question")).toBeEnabled();
  await expect(page.locator("#graph-collection")).toBeEnabled();
});

test("grounded chat defaults to source excerpts and displays cited sources separately from retrieved sources", async ({ page }) => {
  const fixture = await workspace(page);
  const result = groundedResult();
  const unused = { ...hit, chunk_id: "unused", document_id: "unused-doc", title: "Unused source", text: "Unrelated passage." };
  result.retrieval = { ...result.retrieval, hits: [hit, unused] };
  result.citations.push({ ...result.citations[0], label: "S2", chunk_id: "unused", title: "Unused source" });
  await page.route("**/chat", (route) => reply(route, result));
  await ask(page);
  expect(chats(fixture)[0].body).toMatchObject({ context_mode: "question", answer_style: "chat", grounding: "strict" });
  await expect(page.locator(".graph-answer-status")).toHaveText("Answer · 1 cited source");
  await expect(page.locator(".playground-source-chip")).toHaveCount(1);
  await expect(page.locator(".playground-source-chip")).toContainText("S1");
  await page.locator(".graph-chat-evidence > summary").click();
  await expect(page.locator(".graph-chat-evidence-quotes blockquote")).toHaveText(hit.text);
  await expect(page.locator(".graph-chat-sources")).toContainText("Unused source");
  await expect(page.locator(".graph-citation-status")).toContainText("do not verify every claim");
});

test("contextual follow-ups show the actual search query and reruns preserve their original history", async ({ page }) => {
  const fixture = await workspace(page);
  await page.route("**/chat", (route) => {
    const body = route.request().postDataJSON(); const rewritten = body.context_mode === "conversation" && body.history.length > 0;
    return reply(route, groundedResult({ retrieval_query: rewritten ? "Orion sensor operating threshold" : body.text,
      query_context: { mode: body.context_mode, rewritten, duration_ms: rewritten ? 12 : 0, generation: { provider: rewritten ? "openai" : null, model: rewritten ? body.model : null, input_tokens: rewritten ? 35 : 0, output_tokens: rewritten ? 8 : 0 } } }));
  });
  await ask(page, "Tell me about Orion"); await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  await page.locator("#graph-chat-context").selectOption("conversation");
  await expect(page.locator("#graph-chat-context-hint")).toContainText("Provider charges");
  await ask(page, "What threshold does it need?"); await expect(page.locator(".graph-chat-turn")).toHaveCount(2);
  const original = chats(fixture)[1].body;
  expect(original).toMatchObject({ text: "What threshold does it need?", context_mode: "conversation", history: [{ role: "user", content: "Tell me about Orion" }, { role: "assistant", content: "Orion requires threshold 17 [S1]." }] });
  await page.locator(".graph-chat-evidence > summary").last().click();
  await expect(page.locator(".graph-chat-query-trace").last()).toContainText("Orion sensor operating threshold");
  await expect(page.locator(".graph-chat-query-trace").last()).toContainText("Search rewritten");
  const downloadEvent = page.waitForEvent("download"); await page.getByRole("button", { name: "Download diagnostics" }).last().click();
  const download = await downloadEvent; const diagnostic = JSON.parse(await fs.readFile(await download.path(), "utf8"));
  expect(diagnostic).toMatchObject({ question: original.text, retrieval_query: "Orion sensor operating threshold", context_mode: "conversation", answer_style: "chat", grounding: "strict", history: original.history });
  await page.locator("#graph-chat-context").selectOption("question");
  await page.locator("#playground-run-again").click(); await expect(page.locator(".graph-chat-turn")).toHaveCount(3);
  expect(chats(fixture)[2].body).toMatchObject({ context_mode: "question", text: original.text, history: original.history });
  await expect(page.locator("#playground-comparison")).toContainText("Recent conversation");
  await expect(page.locator("#playground-comparison")).toContainText("Current question");
  await expect(page.locator("#playground-comparison")).not.toContainText("Conversation history differs");
});

test("conversation retrieval preflights its rewrite key while current-question keyword retrieval stays local", async ({ page }) => {
  const fixture = await workspace(page, { generationConfigured: false, embeddingConfigured: false, documents: false });
  await page.locator("#playground-strategy").selectOption("keyword");
  await page.locator("#graph-chat-mode").selectOption("retrieve");
  await page.locator("#graph-chat-context").selectOption("conversation");
  await ask(page, "Orion threshold"); await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  expect(chats(fixture)[0].body.history).toEqual([]);
  await ask(page, "What does it mean?"); await expect(page.locator("#graph-chat-status")).toContainText("Conversation search needs an OpenAI API key");
  expect(chats(fixture)).toHaveLength(1);
  await page.locator("#graph-chat-context").selectOption("question");
  await ask(page, "What does the Orion threshold mean?"); await expect(page.locator(".graph-chat-turn")).toHaveCount(2);
  expect(chats(fixture)[1].body).toMatchObject({ context_mode: "question", mode: "retrieve", history: [{ role: "user", content: "Orion threshold" }] });
  await expect(page.locator("#graph-chat-privacy")).toHaveText("Runs locally. No provider calls.");
});

for (const [status, label] of [["insufficient_evidence", "Not enough evidence"], ["invalid_grounding", "Answer withheld"], ["incomplete", "Answer incomplete"], ["refused", "Answer unavailable"], ["no_sources", "No matching sources"]]) {
  test(`grounded chat withholds ${status} drafts from display, speech, diagnostics and subsequent history`, async ({ page }) => {
    const fixture = await workspace(page, { documents: false });
    await page.locator("#graph-chat-delivery").selectOption("voice");
    await page.route("**/chat", (route) => reply(route, groundedResult({ answer_status: status, answer: "UNACCEPTED DRAFT [S1]", speech_text: "UNACCEPTED SPEECH", cited_labels: [], evidence: [], citation_status: "not_applicable" })));
    await ask(page); await expect(page.locator(".graph-answer-status")).toHaveText(label);
    await expect(page.locator("#graph-chat-turns")).not.toContainText("UNACCEPTED");
    await expect(page.getByRole("button", { name: "Copy script" })).toHaveCount(0);
    await expect(page.locator(".playground-source-chip")).toHaveCount(0);
    await page.locator(".graph-chat-evidence > summary").click();
    const downloadEvent = page.waitForEvent("download"); await page.getByRole("button", { name: "Download diagnostics" }).click();
    const download = await downloadEvent; const diagnostic = JSON.parse(await fs.readFile(await download.path(), "utf8"));
    expect(diagnostic.answer).toBeNull(); expect(diagnostic.speech_text).toBeNull();
    await ask(page, "A clearer question"); await expect(page.locator(".graph-chat-turn")).toHaveCount(2);
    expect(chats(fixture)[1].body.history).toEqual([{ role: "user", content: "What threshold does Orion need?" }]);
  });
}

test("clarification questions stay in history without being presented as grounded answers", async ({ page }) => {
  const fixture = await workspace(page);
  await page.route("**/chat", (route) => reply(route, groundedResult({ answer_status: "clarification_needed", answer: "Which Orion model do you mean?", speech_text: "must not appear", citation_status: "not_applicable", cited_labels: [], evidence: [] })));
  await ask(page, "What is its threshold?"); await expect(page.locator(".graph-answer-status")).toHaveText("More detail needed");
  await expect(page.locator(".graph-chat-answer")).toHaveText("Which Orion model do you mean?");
  await expect(page.locator(".playground-source-chip")).toHaveCount(0);
  await ask(page, "The model P00001"); await expect(page.locator(".graph-chat-turn")).toHaveCount(2);
  expect(chats(fixture)[1].body.history).toEqual([{ role: "user", content: "What is its threshold?" }, { role: "assistant", content: "Which Orion model do you mean?" }]);
});

test("voice scripts copy only accepted text and clear on new chat and collection changes", async ({ page }, testInfo) => {
  await page.addInitScript(() => { window.copiedScripts = []; Object.defineProperty(navigator, "clipboard", { value: { writeText: async (text) => { window.copiedScripts.push(text); } }, configurable: true }); });
  await page.setViewportSize({ width: 1512, height: 1050 });
  const fixture = await workspace(page, { documents: false });
  await page.route("**/chat", (route) => reply(route, groundedResult({ answer: "Orion requires threshold 17. [S1]", speech_text: "Orion requires threshold 17." })));
  await page.locator("#graph-chat-delivery").selectOption("voice");
  await ask(page); await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  expect(chats(fixture)[0].body.answer_style).toBe("voice");
  await expect(page.locator(".graph-chat-turn > .graph-chat-answer")).toHaveText("Orion requires threshold 17.");
  await expect(page.locator("#graph-chat-answer-hint")).toContainText("no audio playback");
  await page.getByRole("button", { name: "Copy script" }).click(); await expect(page.locator(".graph-chat-copy-status")).toHaveText("Copied");
  expect(await page.evaluate(() => window.copiedScripts)).toEqual(["Orion requires threshold 17."]);
  await page.locator("#playground-layout").screenshot({ path: testInfo.outputPath("grounded-voice-desktop.png"), style: ".topbar { visibility: hidden; }" });
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.locator("#playground-settings-close").click();
  await page.locator("#graph-chat-panel").screenshot({ path: testInfo.outputPath("grounded-voice-mobile.png"), style: ".topbar { visibility: hidden; }" });
  await page.locator("#graph-chat-clear").click(); await expect(page.getByRole("button", { name: "Copy script" })).toHaveCount(0);
  await ask(page, "Fresh voice question"); await expect(page.getByRole("button", { name: "Copy script" })).toHaveCount(1);
  expect(chats(fixture).at(-1).body.history).toEqual([]);
  await page.locator("#graph-collection").selectOption("archive"); await expect(page.getByRole("button", { name: "Copy script" })).toHaveCount(0);
  expect(await page.evaluate(() => window.copiedScripts)).toHaveLength(1);
});

for (const grounding of ["strict", "standard"]) {
  test(`${grounding} voice scripts preserve the accepted answer's facts, qualifications and punctuation`, async ({ page }) => {
    await page.addInitScript(() => { window.copiedScripts = []; Object.defineProperty(navigator, "clipboard", { value: { writeText: async (text) => { window.copiedScripts.push(text); } }, configurable: true }); });
    const fixture = await workspace(page, { documents: false });
    await page.locator("#graph-chat-grounding").selectOption(grounding);
    await page.locator("#graph-chat-delivery").selectOption("voice");
    const answer = "Orion requires threshold 17 (not 99). [S1]\u0085The threshold is seventeen. [S1]";
    const expected = "Orion requires threshold 17 (not 99). The threshold is seventeen.";
    const invalidScripts = ["Orion requires threshold 99.", "Orion requires threshold 17.", expected.replace("(not 99)", "not 99")];
    for (const [index, speech_text] of invalidScripts.entries()) {
      await page.unroute("**/chat"); await page.route("**/chat", (route) => reply(route, groundedResult({ answer, speech_text })));
      await ask(page, `Voice parity ${index}`); await expect(page.locator(".graph-chat-turn")).toHaveCount(index + 1);
      await expect(page.locator(".graph-answer-status").last()).toHaveText("Answer · 1 cited source");
      await expect(page.locator(".graph-chat-turn > .graph-chat-answer").last()).toHaveText(answer);
      await expect(page.locator(".graph-chat-warning").last()).toContainText("voice script did not match");
      await expect(page.getByRole("button", { name: "Copy script" })).toHaveCount(0);
    }
    await page.locator(".graph-chat-evidence > summary").last().click();
    const downloadEvent = page.waitForEvent("download"); await page.getByRole("button", { name: "Download diagnostics" }).last().click();
    const diagnostic = JSON.parse(await fs.readFile(await (await downloadEvent).path(), "utf8"));
    expect(diagnostic).toMatchObject({ answer, answer_status: "answered", speech_text: null });
    await page.unroute("**/chat"); await page.route("**/chat", (route) => reply(route, groundedResult({ answer, speech_text: expected })));
    await ask(page, "Canonical voice response"); await expect(page.locator(".graph-chat-turn")).toHaveCount(4);
    const history = chats(fixture).at(-1).body.history;
    expect(history.filter((message) => message.role === "assistant").map((message) => message.content)).toEqual([answer, answer, answer]);
    expect(chats(fixture)).toHaveLength(4);
    await expect(page.locator(".graph-chat-turn > .graph-chat-answer").last()).toHaveText(expected);
    await page.getByRole("button", { name: "Copy script" }).click();
    expect(await page.evaluate(() => window.copiedScripts)).toEqual([expected]);
  });
}

test("oversized assistant text is omitted intact while user questions and retrieval-only turns remain bounded", async ({ page }) => {
  const fixture = await workspace(page); let count = 0;
  await page.route("**/chat", (route) => {
    const body = route.request().postDataJSON(); count += 1;
    return reply(route, count === 1 ? groundedResult({ answer: `${"🙂".repeat(2200)} [S1]` }) : body.mode === "retrieve" ? chatResult(null) : groundedResult());
  });
  await ask(page, "Original Orion question"); await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  await page.locator("#graph-chat-mode").selectOption("retrieve");
  await ask(page, "Find Orion sources"); await expect(page.locator(".graph-chat-turn")).toHaveCount(2);
  expect(chats(fixture)[1].body.history).toEqual([{ role: "user", content: "Original Orion question" }]);
  await page.locator(".graph-chat-evidence > summary").last().click();
  await expect(page.locator(".graph-chat-query-trace").last()).toContainText("oversized or older content omitted");
  await page.locator("#graph-chat-mode").selectOption("answer");
  await ask(page, "Explain those sources"); await expect(page.locator(".graph-chat-turn")).toHaveCount(3);
  expect(chats(fixture)[2].body.history).toEqual([{ role: "user", content: "Original Orion question" }, { role: "user", content: "Find Orion sources" }]);
  for (let i = 0; i < 9; i += 1) { await ask(page, `Follow-up ${i}`); await expect.poll(() => chats(fixture).length).toBe(4 + i); await expect(page.locator("#graph-chat-submit")).toBeEnabled(); }
  await expect(page.locator(".graph-chat-turn")).toHaveCount(10);
  for (const call of chats(fixture)) { expect(call.body.history.length).toBeLessThanOrEqual(20); expect(call.body.history.reduce((sum, item) => sum + Buffer.byteLength(item.content), 0)).toBeLessThanOrEqual(32768); expect(call.body.history.every((item) => Buffer.byteLength(item.content) <= 8192)).toBe(true); }
});

test("invalid grounding metadata cannot surface a strict draft and malformed states do not enter history", async ({ page }) => {
  const fixture = await workspace(page);
  await page.route("**/chat", (route) => reply(route, groundedResult({ answer: "BAD DRAFT [S1]", speech_text: "BAD SPEECH", evidence: [{ label: "S1", quote: "a quote absent from this source" }] })));
  await ask(page); await expect(page.locator(".graph-answer-status")).toHaveText("Answer withheld");
  await expect(page.locator("#graph-chat-turns")).not.toContainText("BAD DRAFT");
  await page.locator("#graph-chat-clear").click();
  await page.unroute("**/chat"); await page.route("**/chat", (route) => reply(route, groundedResult({ answer_status: "made_up_state" })));
  await ask(page); await expect(page.locator("#graph-chat-status")).toContainText("invalid chat response"); await expect(page.locator(".graph-chat-turn")).toHaveCount(0);
  await page.unroute("**/chat"); await page.route("**/chat", (route) => reply(route, groundedResult()));
  await ask(page, "Valid question"); await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  expect(chats(fixture).at(-1).body.history).toEqual([]);
});

test("contextual history is cleared by document scope changes, including reruns", async ({ page }) => {
  const fixture = await workspace(page, { documents: false });
  await page.locator("#graph-chat-context").selectOption("conversation");
  await ask(page, "Orion details"); await expect(page.locator(".graph-chat-turn")).toHaveCount(1);
  await ask(page, "What about its threshold?"); await expect(page.locator(".graph-chat-turn")).toHaveCount(2);
  expect(chats(fixture)[1].body.history).toHaveLength(2);
  await page.locator("#graph-filters-panel > summary").click(); await page.locator("#graph-filter-add").click();
  const row = page.locator("[data-graph-filter-row]").last(); await row.locator("[data-filter-column]").selectOption("title"); await row.locator("[data-filter-value]").fill("Manual page 2");
  await page.locator("#playground-run-again").click(); await expect(page.locator(".graph-chat-turn")).toHaveCount(3);
  expect(chats(fixture)[2].body).toMatchObject({ context_mode: "conversation", history: [], retrieval: { document_filters: [{ column: "title", operator: "eq", value: "Manual page 2" }] } });
});

test("strict response validation fails closed for missing metadata, partial evidence and unknown source labels", async ({ page }) => {
  await workspace(page);
  const malformed = groundedResult(); delete malformed.answer_status;
  await page.route("**/chat", (route) => reply(route, malformed));
  await ask(page); await expect(page.locator("#graph-chat-status")).toContainText("invalid chat response"); await expect(page.locator(".graph-chat-turn")).toHaveCount(0);
  const extraHit = { ...hit, chunk_id: "other", text: "This is a different source passage." };
  for (const kind of ["unknown", "partial", "extra", "declared"]) {
    const result = groundedResult({ answer: kind === "unknown" ? "DRAFT [S1] [S99]" : kind === "partial" ? "DRAFT [S1] [S2]" : "DRAFT [S1]" });
    if (kind === "partial") { result.cited_labels.push("S2"); result.retrieval.hits.push(extraHit); result.citations.push({ ...result.citations[0], label: "S2", chunk_id: "other" }); }
    if (kind === "extra") result.evidence.push({ label: "S1", quote: "not in the source" });
    if (kind === "declared") result.cited_labels = [];
    await page.unroute("**/chat"); await page.route("**/chat", (route) => reply(route, result));
    await ask(page, `Check ${kind}`); await expect(page.locator(".graph-answer-status").last()).toHaveText("Answer withheld");
    await expect(page.locator("#graph-chat-turns")).not.toContainText("DRAFT");
  }
});

test("standard mode supports legacy responses but never offers speech with missing or invalid citations", async ({ page }) => {
  await workspace(page, { documents: false }); await page.locator("#graph-chat-grounding").selectOption("standard"); await page.locator("#graph-chat-delivery").selectOption("voice");
  const legacy = chatResult(); for (const key of ["answer_status", "citation_status", "cited_labels", "evidence", "speech_text"]) delete legacy[key];
  await page.route("**/chat", (route) => reply(route, legacy));
  await ask(page); await expect(page.locator(".graph-answer-status")).toHaveText("Answer · 1 cited source");
  for (const [index, citation_status] of ["missing", "invalid"].entries()) {
    await page.unroute("**/chat"); await page.route("**/chat", (route) => reply(route, groundedResult({ answer: citation_status === "missing" ? "Uncited answer" : "Bad label [S99]", citation_status, cited_labels: [], evidence: [], speech_text: "UNSAFE SPEECH" })));
    await ask(page, `${citation_status} citation`); await expect(page.locator(".graph-chat-turn")).toHaveCount(index + 2);
    await expect(page.getByRole("button", { name: "Copy script" })).toHaveCount(0); await expect(page.locator("#graph-chat-turns")).not.toContainText("UNSAFE SPEECH");
  }
});
