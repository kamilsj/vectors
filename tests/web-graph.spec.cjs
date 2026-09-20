"use strict";

const { test, expect } = require("@playwright/test");
const reply = (route, body, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
const deferred = () => { let resolve; const promise = new Promise((done) => { resolve = done; }); return { promise, resolve }; };
const profile = { provider: "openai", model: "text-embedding-3-small", dimensions: 3, context_format_version: 1 };
const collection = (name = "knowledge", count = 18) => ({ config: { name, profile, semantic_neighbors: 3, semantic_threshold: .8 }, revision: 7, document_count: 3, chunk_count: count, edge_count: count, tables: {} });
const titles = ["How retrieval works", "Designing useful context", "Evaluating search results"];
const makeNodes = (count = 18) => Array.from({ length: count }, (_, index) => ({
  chunk_id: `chunk-${index + 1}`, document_id: `document-${Math.floor(index / 6) + 1}`,
  title: titles[Math.floor(index / 6)] || `Document ${Math.floor(index / 6) + 1}`,
  source: `https://example.com/docs/${Math.floor(index / 6) + 1}`,
  text: ["Semantic retrieval finds related ideas even when their vocabulary differs. A graph connects each passage to nearby context and relevant sections from other documents.", "A useful context window keeps the original source, includes enough surrounding detail, and avoids repeating the same evidence.", "Evaluate retrieval with representative questions and inspect the returned sources. Similarity measures a relationship between embeddings, not factual certainty."][index % 3],
  metadata: { section: "Guide" }, start_byte: index * 300, end_byte: index * 300 + 240, ordinal: index % 6,
}));
async function workspace(page, { count = 18, empty = false, dimensions = 3 } = {}) {
  const fixture = { calls: [], nodes: makeNodes(count), revision: 7, collections: empty ? [] : [collection("knowledge", count)],
    edges: [{ from_chunk: "chunk-1", to_chunk: "chunk-2", kind: "adjacent", weight: 1 }, { from_chunk: "chunk-1", to_chunk: "chunk-7", kind: "semantic", weight: .87 }, { from_chunk: "chunk-1", to_chunk: "chunk-13", kind: "supports", weight: .9 }],
    reranking: { provider: "voyage", model: "rerank-2.5", timeout_seconds: 60, max_concurrent_requests: 4, configured: true, persistence: "memory", models: [{ id: "rerank-2.5" }, { id: "rerank-2.5-lite" }] } };
  page.on("request", (request) => { const path = new URL(request.url()).pathname; if (path.startsWith("/v1/")) fixture.calls.push({ path, query: Object.fromEntries(new URL(request.url()).searchParams), method: request.method(), body: request.postData() ? request.postDataJSON() : null, headers: request.headers() }); });
  await page.route("**/healthz", (route) => reply(route, { status: "ok", version: "0.7.0", storage: "memory" }));
  await page.route("**/v1/**", async (route) => {
    const url = new URL(route.request().url()); const path = url.pathname; const method = route.request().method(); const body = route.request().postData() ? route.request().postDataJSON() : null;
    if (path === "/v1/tables") return reply(route, { revision: fixture.revision, tables: [] });
    if (path === "/v1/settings/embeddings") return reply(route, { ...profile, dimensions, configured: true, persistence: "memory", batch_size: 32, timeout_seconds: 60, max_concurrent_requests: 4, providers: [{ id: "openai", label: "OpenAI", configured: true, models: [{ id: profile.model, dimensions: [1536], default_dimensions: 1536 }] }] });
    if (path === "/v1/settings/server") return reply(route, { version: "0.7.0", storage: "memory", authentication: false, compute: {}, limits: {}, capacity: null });
    if (path === "/v1/settings/reranking") { if (method === "PUT") { for (const key of ["model", "timeout_seconds", "max_concurrent_requests"]) fixture.reranking[key] = body[key] ?? fixture.reranking[key]; fixture.reranking.configured = body.clear_api_key ? false : body.api_key ? true : fixture.reranking.configured; } return reply(route, fixture.reranking); }
    if (path === "/v1/graph/collections") { if (method === "POST") { const added = collection(body.name, 0); fixture.collections.push(added); return reply(route, added); } return reply(route, fixture.collections); }
    if (path.endsWith("/neighborhood")) {
      const root = url.searchParams.get("chunk_id");
      const origin = fixture.nodes.find((item) => item.chunk_id === root);
      if (!origin) return reply(route, { error: { code: "chunk_not_found", message: "Passage no longer exists" } }, 404);
      const extra = fixture.nodes.filter((item) => item.chunk_id !== root).slice(-3);
      return reply(route, { collection: "knowledge", revision: fixture.revision, root_chunk: root,
        nodes: [{ ...origin, depth: 0 }, ...extra.map((item, index) => ({ ...item, depth: Number(url.searchParams.get("max_hops")) > 1 && index === 2 ? 2 : 1 }))],
        edges: extra.map((item, index) => ({ from_chunk: index === 1 ? item.chunk_id : root, to_chunk: index === 1 ? root : item.chunk_id, kind: ["semantic", "adjacent", "supports"][index], weight: index === 1 ? 1 : .9 })), truncated: false });
    }
    if (path.endsWith("/graph")) { const offset = Number(url.searchParams.get("offset")); return reply(route, { collection: path.split("/")[4], revision: fixture.revision, nodes: fixture.nodes.slice(offset, offset + 100), edges: fixture.edges, total_nodes: fixture.nodes.length, total_edges: fixture.edges.length, offset, limit: 100, truncated: fixture.nodes.length > 100 }); }
    if (path === "/v1/graph/chunk") return reply(route, { chunks: [{ ordinal: 0, text: body.text, embedding_text: `${body.title}\n\n${body.text}`, byte_start: 0, byte_end: body.text.length, heading: "Introduction" }], embedding_bytes: body.text.length + body.title.length, context_format_version: 1 });
    if (path.endsWith("/documents") && method === "POST") { fixture.revision += 1; return reply(route, { document_id: body.id, revision: fixture.revision, chunks: 1, edges_created: 2, replaced: false, unchanged: false, embedding_usage: { total_tokens: 12 } }); }
    if (path.endsWith("/relationships")) { fixture.revision += 1; if (method === "POST") fixture.edges.push({ from_chunk: body.from_chunk, to_chunk: body.to_chunk, kind: body.kind, weight: body.weight }); else fixture.edges = fixture.edges.filter((edge) => !(edge.from_chunk === body.from_chunk && edge.to_chunk === body.to_chunk && edge.kind === body.kind)); return reply(route, { collection: "knowledge", revision: fixture.revision, created: true, edges_removed: 1 }); }
    if (path.endsWith("/retrieve")) return reply(route, { collection: "knowledge", revision: fixture.revision, hits: fixture.nodes.slice(0, 3).map((item, index) => ({ ...item, similarity: .89 - index / 10, lexical_score: .3, fusion_score: .028, rerank_score: body.reranker === "voyage" ? .78 : null, selection_score: .6, depth: index ? 1 : 0, seed: index === 0 })), edges: fixture.edges, truncated: false, context_bytes: 700, candidate_count: 18, lexical_cache_hit: false, reranking: { method: body.reranker, model: body.reranker === "voyage" ? "rerank-2.5" : null, total_tokens: 30 }, embedding_usage: { total_tokens: 4 } });
    return reply(route, { error: { code: "not_found", message: "Unexpected graph test request" } }, 404);
  });
  await page.goto("/"); await expect(page.locator("#status-label")).toHaveText("Connected");
  return fixture;
}
async function openGraph(page) { await page.locator('.nav-item[data-view="connections"]').click(); await expect(page.locator("#view-title")).toHaveText("Connections"); }
async function selectFirst(page) { await page.locator(".graph-point").first().click(); await expect(page.locator("#graph-details")).toContainText("chunk-1"); }

test("graph draws bounded SVG nodes, accessible list, kinds, selection and source citations", async ({ page }) => {
  const fixture = await workspace(page); await openGraph(page);
  await expect(page.locator(".graph-point")).toHaveCount(18);
  await expect(page.locator(".graph-list-node")).toHaveCount(18);
  await expect(page.locator(".graph-edge.custom")).toHaveCount(1);
  await page.locator('.graph-point').first().focus(); await page.keyboard.press("Enter");
  await expect(page.locator("#graph-details")).toContainText("UTF-8 bytes 0–240");
  await expect(page.locator("#graph-details a")).toHaveAttribute("href", "https://example.com/docs/1");
  await page.locator('[data-graph-kind="semantic"]').uncheck(); await expect(page.locator(".graph-edge.semantic")).toBeHidden();
  await page.getByRole("button", { name: "Zoom in graph", exact: true }).click(); await expect(page.locator("#graph-zoom-reset")).toHaveText("1.3×");
  expect(fixture.calls.some((call) => call.method === "POST" && /\/retrieve$|\/documents$|\/embeddings$/.test(call.path))).toBe(false);
});

test("100 node page remains bounded and requests the next server page", async ({ page }) => {
  await workspace(page, { count: 205 }); await openGraph(page);
  await expect(page.locator(".graph-point")).toHaveCount(100);
  await expect(page.locator("#graph-range")).toHaveText("1–100 of 205 passages");
  await page.getByRole("button", { name: "Next graph page" }).click(); await expect(page.locator("#graph-range")).toHaveText("101–200 of 205 passages");
  await page.getByRole("button", { name: "Next graph page" }).click(); await expect(page.locator(".graph-point")).toHaveCount(5);
  await expect(page.getByRole("button", { name: "Next graph page" })).toBeDisabled();
});

test("empty collections can be created without provider generation", async ({ page }) => {
  const fixture = await workspace(page, { empty: true, count: 0 }); await openGraph(page);
  await expect(page.locator("#graph-empty")).toBeVisible();
  await page.locator("#graph-create-open").click(); await page.locator("#graph-create-name").fill("research"); await page.locator("#graph-create-submit").click();
  await expect(page.locator("#graph-collection")).toHaveValue("research"); await expect(page.locator("#graph-status")).toContainText("Collection created");
  expect(fixture.calls.find((call) => call.path === "/v1/graph/collections" && call.method === "POST").body).toEqual({ name: "research", semantic_neighbors: 3, semantic_threshold: .8 });
  expect(fixture.calls.filter((call) => call.method === "POST" && /\/embeddings$|\/documents$/.test(call.path))).toHaveLength(0);
});

test("preview is lazy and saving a document uses the known revision", async ({ page }) => {
  const fixture = await workspace(page); await openGraph(page); await expect(page.locator(".graph-point")).toHaveCount(18);
  await page.locator("#graph-add-open").click(); await page.locator("#graph-document-id").fill("new-guide"); await page.locator("#graph-document-title").fill("New guide"); await page.locator("#graph-document-text").fill("A passage about retrieval and context.");
  await page.locator("#graph-preview").click(); await expect(page.locator("#graph-document-status")).toContainText("1 chunks");
  await expect(page.locator("#graph-chunk-preview pre")).toHaveCount(0); await page.locator("#graph-chunk-preview summary").click(); await expect(page.locator("#graph-chunk-preview pre")).toContainText("New guide");
  expect(fixture.calls.filter((call) => call.path.endsWith("/documents"))).toHaveLength(0);
  await page.locator("#graph-document-save").click(); await expect(page.locator("#graph-document-status")).toContainText("Saved document");
  expect(fixture.calls.find((call) => call.path.endsWith("/documents")).body).toMatchObject({ id: "new-guide", expected_revision: 7, title: "New guide", chunking: { max_characters: 1200, overlap_characters: 150, max_chunks: 256 } });
});

test("profile mismatch stops a paid request", async ({ page }) => {
  const fixture = await workspace(page, { dimensions: 2 }); await openGraph(page); await page.locator("#graph-question").fill("What connects these ideas?"); await page.locator("#graph-search-submit").click();
  await expect(page.locator("#graph-search-status")).toContainText("Restore that profile"); expect(fixture.calls.some((call) => call.path.endsWith("/retrieve"))).toBe(false);
});

test("balanced hybrid retrieval reports score types and optional Voyage privacy", async ({ page }) => {
  const fixture = await workspace(page); await openGraph(page); await page.locator("#graph-question").fill("How should I evaluate context?"); await page.locator("#graph-search-submit").click();
  await expect(page.locator(".graph-hit")).toHaveCount(3); await expect(page.locator("#graph-search-results")).toContainText("Local hybrid ranking"); await expect(page.locator("#graph-search-status")).toContainText("not confidence");
  const local = fixture.calls.find((call) => call.path.endsWith("/retrieve")); expect(local.body).toMatchObject({ candidate_limit: 40, seed_limit: 12, max_results: 10, max_hops: 1, neighbor_limit: 8, reranker: "local", diversity: .3, max_context_bytes: 24000, max_per_document: 3, vector_weight: 1, lexical_weight: 1 });
  await page.locator("#graph-reranker").selectOption("voyage"); await expect(page.locator("#graph-search-privacy")).toContainText("candidate passage context"); await page.locator("#graph-search-submit").click(); await expect(page.locator("#graph-search-results")).toContainText("Voyage rerank-2.5"); await expect(page.locator(".graph-scores").first()).toContainText("Rerank 0.7800");
});

test("relationship creation and deletion carry the displayed revision", async ({ page }) => {
  const fixture = await workspace(page); await openGraph(page); await selectFirst(page);
  await page.locator("#graph-link-target").selectOption("chunk-3"); await page.locator("#graph-link-kind").fill("references"); await page.locator("#graph-link-save").click(); await expect(page.locator("#graph-status")).toHaveText("Directed relationship saved.");
  expect(fixture.calls.find((call) => call.path.endsWith("/relationships") && call.method === "POST").body).toEqual({ from_chunk: "chunk-1", to_chunk: "chunk-3", kind: "references", weight: 1, expected_revision: 7 });
  await page.getByRole("button", { name: "Remove references relationship to chunk-3", exact: true }).click(); await expect(page.locator("#graph-status")).toHaveText("Directed relationship removed.");
  expect(fixture.calls.find((call) => call.path.endsWith("/relationships") && call.method === "DELETE").body).toEqual({ from_chunk: "chunk-1", to_chunk: "chunk-3", kind: "references", expected_revision: 8 });
});

test("revision conflicts preserve the relationship draft and never retry", async ({ page }) => {
  const fixture = await workspace(page); await page.route("**/relationships", (route) => reply(route, { error: { code: "stale_revision", message: "Changed" } }, 409)); await openGraph(page); await selectFirst(page);
  await page.locator("#graph-link-target").selectOption("chunk-3"); await page.locator("#graph-link-kind").fill("supports"); await page.locator("#graph-link-save").click();
  await expect(page.locator("#graph-status")).toContainText("draft is preserved"); await expect(page.locator("#graph-link-target")).toHaveValue("chunk-3"); await expect(page.locator("#graph-link-kind")).toHaveValue("supports");
  expect(fixture.calls.filter((call) => call.path.endsWith("/relationships"))).toHaveLength(1);
});

test("untrusted titles, passage text, and source schemes stay inert", async ({ page }) => {
  const fixture = await workspace(page); fixture.nodes[0] = { ...fixture.nodes[0], title: '<img src=x onerror="window.graphXss=1">', text: '<script>window.graphXss=2</script>', source: "javascript:window.graphXss=3" }; await openGraph(page); await selectFirst(page);
  await expect(page.locator("#graph-details")).toContainText('<script>window.graphXss=2</script>'); await expect(page.locator("#graph-details a")).toHaveCount(0); expect(await page.evaluate(() => window.graphXss)).toBeUndefined();
});

test("a late graph page cannot replace a newer selection", async ({ page }) => {
  const fixture = await workspace(page); fixture.collections.push(collection("other")); const gate = deferred();
  await page.route("**/collections/knowledge/graph?*", async (route) => { await gate.promise; await reply(route, { collection: "knowledge", revision: 7, nodes: [{ ...fixture.nodes[0], title: "Obsolete response" }], edges: [], total_nodes: 1, total_edges: 0, offset: 0, limit: 100 }); });
  await openGraph(page); await expect(page.locator("#graph-collection")).toHaveValue("knowledge"); await page.locator("#graph-collection").selectOption("other"); await expect(page.locator(".graph-point")).toHaveCount(18); gate.resolve();
  await expect(page.locator("#graph-collection")).toHaveValue("other"); await expect(page.locator("#graph-legend")).not.toContainText("Obsolete response");
});

test("token changes clear graph drafts, keys and stale retrieval results", async ({ page }) => {
  await workspace(page); const gate = deferred(); let entered = false;
  await page.route("**/retrieve", async (route) => { entered = true; await gate.promise; await reply(route, { hits: [{ text: "Old secret result" }], reranking: { method: "local" }, candidate_count: 1, context_bytes: 17 }); });
  await openGraph(page); await page.locator("#graph-question").fill("Private question"); await page.locator("#graph-search-submit").click(); await expect.poll(() => entered).toBe(true);
  await page.evaluate(() => { document.querySelector("#rerank-api-key").value = "private-rerank-key"; document.querySelector("#graph-document-text").value = "private draft"; });
  await page.locator("#open-token").click(); await page.locator("#token-input").fill("new-session"); await page.locator("#save-token").click(); gate.resolve();
  await expect(page.locator("#graph-question")).toHaveValue(""); await expect(page.locator("#graph-document-text")).toHaveValue(""); await expect(page.locator("#rerank-api-key")).toHaveValue(""); await expect(page.locator("#graph-search-results")).not.toContainText("Old secret result");
});

test("reranking settings write keys once without browser persistence", async ({ page }) => {
  const fixture = await workspace(page); await page.locator('.nav-item[data-view="settings"]').click(); await expect(page.locator("#rerank-key-status")).toContainText("active reranking key");
  await page.locator("#rerank-api-key").fill("synthetic-rerank-secret"); await page.locator("#rerank-model").selectOption("rerank-2.5-lite"); await page.locator("#rerank-save").click(); await expect(page.locator("#rerank-settings-status")).toContainText("saved"); await expect(page.locator("#rerank-api-key")).toHaveValue("");
  expect(fixture.calls.find((call) => call.path === "/v1/settings/reranking" && call.method === "PUT").body).toMatchObject({ model: "rerank-2.5-lite", api_key: "synthetic-rerank-secret" });
  expect(await page.evaluate(() => JSON.stringify({ ...localStorage, ...sessionStorage }))).not.toContain("synthetic-rerank-secret");
  await page.locator("#rerank-clear-key").check(); await page.locator("#rerank-save").click(); await expect(page.locator("#rerank-key-status")).toHaveText("No active reranking key.");
});

test("auth failures surface the connection dialog and graph errors remain visible", async ({ page }) => {
  await workspace(page); await page.route("**/v1/graph/collections", (route) => reply(route, { error: { code: "unauthorized", message: "An API token is required" } }, 401)); await openGraph(page); await expect(page.locator("#token-dialog")).toBeVisible(); await expect(page.locator("#graph-status")).toContainText("API token");
  await page.getByRole("button", { name: "Close", exact: true }).click(); await page.route("**/v1/graph/collections", (route) => reply(route, { error: { code: "database_busy", message: "Graph capacity is busy; try again later" } }, 503)); await page.locator("#graph-refresh").click(); await expect(page.locator("#graph-status")).toContainText("capacity is busy");
});

test("Connections stays within desktop and mobile viewport widths", async ({ page }) => {
  await page.setViewportSize({ width: 1512, height: 1050 }); await workspace(page); await openGraph(page); await selectFirst(page);
  await page.screenshot({ path: "/private/tmp/vectors-connections-desktop.png", fullPage: true });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.setViewportSize({ width: 390, height: 844 }); await page.screenshot({ path: "/private/tmp/vectors-connections-mobile.png", fullPage: true });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await expect(page.locator("#graph-details")).toContainText("chunk-1");
});

test("a provider failure preserves the document draft without automatic retries", async ({ page }) => {
  const fixture = await workspace(page); await page.route("**/collections/knowledge/documents", (route) => reply(route, { error: { code: "provider_error", message: "Embedding provider unavailable" } }, 502));
  await openGraph(page); await expect(page.locator(".graph-point")).toHaveCount(18); await page.locator("#graph-add-open").click();
  await page.locator("#graph-document-id").fill("keep-me"); await page.locator("#graph-document-text").fill("Keep this draft when generation fails."); await page.locator("#graph-document-save").click();
  await expect(page.locator("#graph-document-status")).toContainText("provider unavailable"); await expect(page.locator("#graph-document-text")).toHaveValue("Keep this draft when generation fails.");
  expect(fixture.calls.filter((call) => call.path.endsWith("/documents"))).toHaveLength(1); await expect(page.locator("#graph-document-save")).toBeEnabled();
});

test("changing connection during a graph write retains its uncertain outcome notice", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await page.route("**/relationships", async (route) => { entered = true; await gate.promise; await reply(route, { revision: 8, created: true }); });
  await openGraph(page); await selectFirst(page); await page.locator("#graph-link-target").selectOption("chunk-3"); await page.locator("#graph-link-save").click(); await expect.poll(() => entered).toBe(true);
  await expect(page.locator("#graph-collection")).toBeDisabled(); await page.locator("#open-token").click(); await page.locator("#token-input").fill("replacement-token"); await page.locator("#save-token").click(); gate.resolve();
  await expect(page.locator(".graph-point")).toHaveCount(18); await expect(page.locator("#graph-status")).toContainText("may still finish on the server"); expect(fixture.calls.filter((call) => call.path.endsWith("/relationships"))).toHaveLength(1);
});

test("empty retrieval is explicit and impossible result limits do not contact a provider", async ({ page }) => {
  const fixture = await workspace(page); await openGraph(page); await page.locator("#graph-question").fill("Empty context"); await page.locator("#graph-search-form summary").click(); await page.locator("#graph-candidates").fill("2"); await page.locator("#graph-search-submit").click();
  await expect(page.locator("#graph-search-status")).toContainText("must not exceed"); expect(fixture.calls.some((call) => call.path.endsWith("/retrieve"))).toBe(false);
  await page.locator("#graph-candidates").fill("40"); await page.route("**/retrieve", (route) => reply(route, { hits: [], candidate_count: 0, context_bytes: 0, reranking: { method: "local", model: null }, truncated: false })); await page.locator("#graph-search-submit").click(); await expect(page.locator("#graph-search-results")).toContainText("No passages fit");
});

test("focused exploration crosses page boundaries and restores the previous page and selection", async ({ page }) => {
  const fixture = await workspace(page, { count: 205 }); await openGraph(page);
  await page.getByRole("button", { name: "Next graph page" }).click(); await expect(page.locator("#graph-range")).toHaveText("101–200 of 205 passages");
  await page.locator('.graph-point[data-chunk="chunk-101"]').focus(); await page.keyboard.press("Enter");
  await page.locator("#graph-details").getByRole("button", { name: "Explore connections", exact: true }).click();
  await expect(page.locator("#graph-mode")).toHaveText("FOCUSED CONNECTIONS");
  await expect(page.locator('.graph-point[data-chunk="chunk-205"]')).toHaveCount(1);
  await expect(page.locator(".graph-point.graph-root")).toHaveAttribute("data-chunk", "chunk-101");
  await expect(page.locator("#graph-range")).toHaveText("4 passages in focused view");
  await expect(page.locator("#graph-previous")).toBeHidden();
  const call = fixture.calls.find((item) => item.path.endsWith("/neighborhood"));
  expect(call.query).toEqual({ chunk_id: "chunk-101", max_hops: "1", direction: "both", min_weight: "0", neighbor_limit: "8", max_nodes: "100", max_edges: "500" });
  expect(fixture.calls.filter((item) => item.method === "POST")).toHaveLength(0);
  await page.getByRole("button", { name: "Back to all passages", exact: false }).click();
  await expect(page.locator("#graph-mode")).toHaveText("ALL PASSAGES"); await expect(page.locator("#graph-range")).toHaveText("101–200 of 205 passages");
  await expect(page.locator('.graph-point[data-chunk="chunk-101"]')).toHaveAttribute("aria-pressed", "true");
  await expect(page.locator("#graph-neighborhood-controls")).toBeHidden();
});

test("a retrieval result can explore an off-page passage with its exact encoded identifier", async ({ page }) => {
  const fixture = await workspace(page, { count: 205 });
  const hit = { ...fixture.nodes[204], chunk_id: "special /?#& passage", ordinal: undefined };
  fixture.nodes[204] = hit;
  await page.route("**/retrieve", (route) => reply(route, { hits: [{ ...hit, similarity: .8, depth: 0, seed: true }], candidate_count: 1, context_bytes: 240, reranking: { method: "local", model: null }, truncated: false }));
  await openGraph(page); await page.locator("#graph-question").fill("Find a distant source"); await page.locator("#graph-search-submit").click();
  await page.locator(".graph-hit").getByRole("button", { name: "Explore connections", exact: true }).click();
  await expect(page.locator("#graph-focus-title")).toContainText("bytes 61200–61440");
  await expect(page.locator(".graph-point.graph-root")).toHaveAttribute("data-chunk", hit.chunk_id);
  expect(fixture.calls.find((item) => item.path.endsWith("/neighborhood")).query.chunk_id).toBe(hit.chunk_id);
  await expect(page.locator("#graph-details")).toContainText("Root passage");
});

test("neighborhood filters query direction and weight while edge-type checkboxes stay local", async ({ page }) => {
  const fixture = await workspace(page); await openGraph(page); await selectFirst(page);
  await page.locator("#graph-details").getByRole("button", { name: "Explore connections", exact: true }).click();
  await expect(page.locator(".graph-point.graph-root")).toHaveCount(1);
  await page.locator("#graph-neighborhood-hops").selectOption("2"); await page.locator("#graph-neighborhood-direction").selectOption("incoming"); await page.locator("#graph-neighborhood-min-weight").fill("0.7");
  await page.locator("#graph-neighborhood-apply").click(); await expect(page.locator("#graph-status")).toContainText("2 hops · incoming");
  const calls = fixture.calls.filter((item) => item.path.endsWith("/neighborhood")); expect(calls).toHaveLength(2); expect(calls[1].query).toMatchObject({ max_hops: "2", direction: "incoming", min_weight: "0.7" });
  await expect(page.locator(".graph-point[data-depth='2']")).toHaveCount(1);
  await expect(page.locator("#graph-node-list")).toContainText("2 hops");
  const incoming = page.locator('.graph-edge[data-from="chunk-17"][data-to="chunk-1"]'); await expect(incoming).toHaveAttribute("marker-end", "url(#graph-arrow)");
  await page.locator('[data-graph-kind="semantic"]').uncheck(); await expect(page.locator(".graph-edge.semantic")).toBeHidden(); expect(fixture.calls.filter((item) => item.path.endsWith("/neighborhood"))).toHaveLength(2);
});

test("relationship changes in focused mode use its revision and refresh the same neighborhood", async ({ page }) => {
  const fixture = await workspace(page); fixture.revision = 22; await openGraph(page); await selectFirst(page);
  await page.locator("#graph-details").getByRole("button", { name: "Explore connections", exact: true }).click(); await expect(page.locator(".graph-point.graph-root")).toHaveCount(1);
  await page.locator("#graph-link-target").selectOption("chunk-18"); await page.locator("#graph-link-save").click(); await expect(page.locator("#graph-status")).toHaveText("Directed relationship saved.");
  const call = fixture.calls.find((item) => item.path.endsWith("/relationships")); expect(call.body).toMatchObject({ from_chunk: "chunk-1", to_chunk: "chunk-18", expected_revision: 22 });
  await expect(page.locator("#graph-mode")).toHaveText("FOCUSED CONNECTIONS"); expect(fixture.calls.filter((item) => item.path.endsWith("/neighborhood"))).toHaveLength(2);
});

test("returning to pages rejects a late neighborhood response", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await page.route("**/neighborhood?*", async (route) => { entered = true; await gate.promise; await reply(route, { collection: "knowledge", revision: 7, root_chunk: "chunk-1", nodes: [{ ...fixture.nodes[0], title: "Obsolete focused result", depth: 0 }], edges: [], truncated: false }); });
  await openGraph(page); await selectFirst(page); await page.locator("#graph-details").getByRole("button", { name: "Explore connections", exact: true }).click(); await expect.poll(() => entered).toBe(true);
  await page.locator("#graph-back-pages").click(); await expect(page.locator(".graph-point")).toHaveCount(18); gate.resolve();
  await expect(page.locator("#graph-mode")).toHaveText("ALL PASSAGES"); await expect(page.locator("#graph-legend")).not.toContainText("Obsolete focused result");
});

test("a newer neighborhood intent wins over a delayed filter response", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await openGraph(page); await selectFirst(page); await page.locator("#graph-details").getByRole("button", { name: "Explore connections", exact: true }).click(); await expect(page.locator(".graph-point")).toHaveCount(4);
  await page.route("**/neighborhood?*", async (route) => {
    const query = new URL(route.request().url()).searchParams;
    if (query.get("direction") !== "incoming") return route.fallback();
    entered = true; await gate.promise; await reply(route, { collection: "knowledge", revision: 7, root_chunk: "chunk-1", nodes: [{ ...fixture.nodes[0], title: "Old incoming response", depth: 0 }], edges: [], truncated: false });
  });
  await page.locator("#graph-neighborhood-direction").selectOption("incoming"); await page.locator("#graph-neighborhood-apply").click(); await expect.poll(() => entered).toBe(true);
  await page.locator("#graph-neighborhood-direction").selectOption("outgoing"); await page.locator("#graph-neighborhood-apply").click(); await expect(page.locator("#graph-status")).toContainText("outgoing"); gate.resolve();
  await expect(page.locator(".graph-point")).toHaveCount(4); await expect(page.locator("#graph-focus-title")).not.toContainText("Old incoming response");
});

test("token changes clear focused state and a stale unauthorized neighborhood cannot reopen authentication", async ({ page }) => {
  await workspace(page); const gate = deferred(); let entered = false;
  await page.route("**/neighborhood?*", async (route) => { entered = true; await gate.promise; await reply(route, { error: { code: "unauthorized", message: "Expired old token" } }, 401); });
  await openGraph(page); await selectFirst(page); await page.locator("#graph-details").getByRole("button", { name: "Explore connections", exact: true }).click(); await expect.poll(() => entered).toBe(true);
  await page.locator("#open-token").click(); await page.locator("#token-input").fill("new-neighborhood-token"); await page.locator("#save-token").click(); gate.resolve();
  await expect(page.locator(".graph-point")).toHaveCount(18); await expect(page.locator("#graph-mode")).toHaveText("ALL PASSAGES"); await expect(page.locator("#graph-back-pages")).toBeHidden(); await expect(page.locator("#token-dialog")).not.toBeVisible();
});

test("neighborhood authorization and missing-passage errors preserve a route back to the collection", async ({ page }) => {
  await workspace(page); await page.route("**/neighborhood?*", (route) => reply(route, { error: { code: "unauthorized", message: "Neighborhood token required" } }, 401));
  await openGraph(page); await selectFirst(page); await page.locator("#graph-details").getByRole("button", { name: "Explore connections", exact: true }).click(); await expect(page.locator("#token-dialog")).toBeVisible();
  await page.getByRole("button", { name: "Close", exact: true }).click(); await expect(page.locator("#graph-status")).toContainText("Neighborhood token required");
  await page.route("**/neighborhood?*", (route) => reply(route, { error: { code: "chunk_not_found", message: "Passage no longer exists" } }, 404)); await page.locator("#graph-neighborhood-apply").click(); await expect(page.locator("#graph-status")).toContainText("Passage no longer exists");
  await page.locator("#graph-back-pages").click(); await expect(page.locator(".graph-point")).toHaveCount(18);
});

test("focused graph has readable root and depth labels at desktop and mobile widths", async ({ page }) => {
  await page.setViewportSize({ width: 1512, height: 1050 }); await workspace(page); await openGraph(page); await selectFirst(page);
  await page.locator("#graph-details").getByRole("button", { name: "Explore connections", exact: true }).click(); await page.locator("#graph-neighborhood-hops").selectOption("2"); await page.locator("#graph-neighborhood-apply").click();
  await expect(page.locator(".graph-point.graph-root text")).toContainText("How retrieval works"); await expect(page.locator(".graph-point.graph-root text")).toContainText("root");
  await page.screenshot({ path: "/private/tmp/vectors-neighborhood-desktop.png", fullPage: true }); expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.setViewportSize({ width: 390, height: 844 }); await page.screenshot({ path: "/private/tmp/vectors-neighborhood-mobile.png", fullPage: true }); expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.locator(".graph-accessible-list summary").click(); await page.locator(".graph-list-node").last().focus(); await page.keyboard.press("Enter"); await expect(page.locator("#graph-details")).toContainText("2 hops from root");
});

const pathResult = (hits) => ({ collection: "knowledge", revision: 7, hits, edges: [], truncated: false, context_bytes: 700, candidate_count: 18, lexical_cache_hit: false, reranking: { method: "local", model: null, total_tokens: 0 }, embedding_usage: { total_tokens: 4 } });
const pathHit = (item, path) => ({ ...item, similarity: .82, lexical_score: .2, fusion_score: .024, selection_score: .5, seed: false, depth: path?.edges?.length || 1, ...(path === undefined ? {} : { retrieval_path: path }) });
async function askGraph(page, question = "How are these passages connected?") {
  await page.locator("#graph-question").fill(question); await page.locator("#graph-search-submit").click();
}

test("retrieval sends direction, exact kind and weight with the explicit seed budget", async ({ page }) => {
  const fixture = await workspace(page); await openGraph(page); await page.locator("#graph-search-form summary").click();
  await page.locator("#graph-retrieval-direction").selectOption("incoming"); await page.locator("#graph-retrieval-kind").fill("supports"); await page.locator("#graph-retrieval-min-weight").fill("0.65"); await page.locator("#graph-seeds").fill("4");
  await askGraph(page); await expect(page.locator(".graph-hit")).toHaveCount(3);
  expect(fixture.calls.find((call) => call.path.endsWith("/retrieve")).body).toMatchObject({ direction: "incoming", kind: "supports", min_weight: .65, seed_limit: 4 });
});

test("small candidate budgets reserve graph context while explicit seeds remain unchanged", async ({ page }) => {
  const fixture = await workspace(page); await openGraph(page); await page.locator("#graph-search-form summary").click();
  await page.locator("#graph-candidates").fill("8"); await expect(page.locator("#graph-seeds")).toHaveValue("6"); await page.locator("#graph-result-limit").fill("4"); await askGraph(page); await expect(page.locator(".graph-hit")).toHaveCount(3);
  expect(fixture.calls.find((call) => call.path.endsWith("/retrieve")).body).toMatchObject({ candidate_limit: 8, seed_limit: 6 });
  await page.locator("#graph-seeds").fill("7"); await page.locator("#graph-candidates").fill("4"); await expect(page.locator("#graph-seeds")).toHaveValue("7"); await askGraph(page); await expect(page.locator("#graph-search-status")).toContainText("Starting passages must not exceed");
  expect(fixture.calls.filter((call) => call.path.endsWith("/retrieve"))).toHaveLength(1);
  await page.locator("#graph-seeds-auto").click(); await expect(page.locator("#graph-seeds")).toHaveValue("3");
  await page.locator("#graph-seeds").fill("4"); await expect(page.locator("#graph-seed-hint")).toContainText("Every candidate slot is a seed");
  await page.locator("#graph-candidates").fill("40"); await expect(page.locator("#graph-seeds")).toHaveValue("4");
});

test("invalid relationship filters and seed limits stop before retrieval", async ({ page }) => {
  const fixture = await workspace(page); await openGraph(page); await page.locator("#graph-search-form summary").click();
  await page.locator("#graph-retrieval-kind").fill("Invalid label"); await askGraph(page); await expect(page.locator("#graph-search-status")).toContainText("Relationship type must start");
  await page.locator("#graph-retrieval-kind").fill(""); await page.locator("#graph-retrieval-min-weight").fill("1.2"); await askGraph(page); await expect(page.locator("#graph-search-status")).toContainText("Minimum relationship weight");
  await page.locator("#graph-retrieval-min-weight").fill("0"); await page.locator("#graph-seeds").fill("0"); await askGraph(page); await expect(page.locator("#graph-search-status")).toContainText("Starting passages must be a whole number");
  await page.locator("#graph-seeds").fill("4"); await page.evaluate(() => { const select = document.querySelector("#graph-retrieval-direction"); const option = document.createElement("option"); option.value = "sideways"; select.append(option); select.value = "sideways"; }); await askGraph(page); await expect(page.locator("#graph-search-status")).toContainText("valid relationship direction");
  expect(fixture.calls.some((call) => call.path.endsWith("/retrieve"))).toBe(false);
});

test("legacy retrieval responses omit path explanations and empty kind remains optional", async ({ page }) => {
  const fixture = await workspace(page); await openGraph(page); await askGraph(page); await expect(page.locator(".graph-hit")).toHaveCount(3); await expect(page.locator(".graph-retrieval-path")).toHaveCount(0);
  const payload = fixture.calls.find((call) => call.path.endsWith("/retrieve")).body;
  expect(payload).toMatchObject({ direction: "outgoing", min_weight: 0, seed_limit: 12 }); expect(payload).not.toHaveProperty("kind");
});

test("a two-hop explanation preserves incoming arrows and identifies unreturned bridges", async ({ page }) => {
  const fixture = await workspace(page);
  const path = { seed_chunk_id: "chunk-1", edges: [ { from_chunk: "chunk-7", to_chunk: "chunk-1", kind: "supports", weight: .9 }, { from_chunk: "chunk-7", to_chunk: "chunk-13", kind: "references", weight: .75 } ] };
  await page.route("**/retrieve", (route) => reply(route, pathResult([pathHit(fixture.nodes[12], path)])));
  await openGraph(page); await askGraph(page); await expect(page.locator(".graph-retrieval-path")).toHaveCount(1); await expect(page.locator(".graph-path-steps")).toHaveCount(0);
  await page.getByText("How this passage was found", { exact: true }).click(); await expect(page.locator(".graph-path-seed")).toContainText("chunk-1");
  await expect(page.locator(".graph-path-steps > li")).toHaveCount(2); await expect(page.locator(".graph-path-arrow").first()).toHaveText("←"); await expect(page.locator(".graph-path-arrow").last()).toHaveText("→");
  await expect(page.locator(".graph-path-steps")).toContainText("weight 0.9000 · followed incoming"); await expect(page.locator(".graph-path-steps")).toContainText("Connection identifier · source passage not returned");
  await page.getByRole("button", { name: "Explore connections for chunk-7", exact: true }).click(); await expect(page.locator(".graph-point.graph-root")).toHaveAttribute("data-chunk", "chunk-7");
});

test("invalid or disconnected explanations stay bounded and do not claim a path", async ({ page }) => {
  const fixture = await workspace(page);
  const edge = { from_chunk: "chunk-1", to_chunk: "chunk-2", kind: "supports", weight: .8 };
  const paths = [
    { seed_chunk_id: "chunk-1", edges: [edge] },
    { seed_chunk_id: "chunk-1", edges: [{ ...edge, from_chunk: "unrelated", to_chunk: "chunk-13" }] },
    { seed_chunk_id: "chunk-1", edges: Array(4).fill(edge) },
    { seed_chunk_id: "chunk-1", edges: [{ ...edge, to_chunk: "chunk-13", weight: 1.1 }] },
  ];
  await page.route("**/retrieve", (route) => reply(route, pathResult(paths.map((path) => pathHit(fixture.nodes[12], path)))));
  await openGraph(page); await askGraph(page); await expect(page.locator(".graph-retrieval-path")).toHaveCount(4);
  for (const summary of await page.locator(".graph-retrieval-path summary").all()) await summary.click();
  await expect(page.locator(".graph-path-steps")).toHaveCount(0); await expect(page.getByText("The server returned an incomplete connection explanation.", { exact: false })).toHaveCount(4);
  await expect(page.locator(".graph-hit")).toHaveCount(4);
});

test("untrusted path identifiers are text and never become HTML or executable links", async ({ page }) => {
  const fixture = await workspace(page); const hostile = '<img src=x onerror="window.pathXss=1">';
  const path = { seed_chunk_id: hostile, edges: [{ from_chunk: hostile, to_chunk: "chunk-13", kind: "supports", weight: .8 }] };
  await page.route("**/retrieve", (route) => reply(route, pathResult([pathHit(fixture.nodes[12], path)])));
  await openGraph(page); await askGraph(page); await page.getByText("How this passage was found", { exact: true }).click();
  await expect(page.locator(".graph-path-seed code")).toHaveText(hostile); await expect(page.locator(".graph-retrieval-path img, .graph-retrieval-path a")).toHaveCount(0); expect(await page.evaluate(() => window.pathXss)).toBeUndefined();
});

test("a stale path response after reconnect cannot restore old provenance", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await page.route("**/retrieve", async (route) => { entered = true; await gate.promise; await reply(route, pathResult([pathHit(fixture.nodes[12], { seed_chunk_id: "old-private-seed", edges: [{ from_chunk: "old-private-seed", to_chunk: "chunk-13", kind: "supports", weight: 1 }] })])); });
  await openGraph(page); await askGraph(page); await expect.poll(() => entered).toBe(true);
  await page.locator("#open-token").click(); await page.locator("#token-input").fill("new-path-session"); await page.locator("#save-token").click(); gate.resolve();
  await expect(page.locator("#status-label")).toHaveText("Connected"); await expect(page.locator(".graph-retrieval-path")).toHaveCount(0); await expect(page.locator("#graph-search-results")).not.toContainText("old-private-seed");
  await expect(page.locator("#graph-seeds")).toHaveValue("12"); await expect(page.locator("#graph-retrieval-direction")).toHaveValue("outgoing");
});

test("retrieval path details and advanced controls fit desktop and mobile screens", async ({ page }) => {
  await page.setViewportSize({ width: 1512, height: 1050 }); const fixture = await workspace(page);
  const path = { seed_chunk_id: "chunk-1", edges: [{ from_chunk: "chunk-7", to_chunk: "chunk-1", kind: "supports", weight: .9 }, { from_chunk: "chunk-7", to_chunk: "chunk-13", kind: "references", weight: .75 }] };
  await page.route("**/retrieve", (route) => reply(route, pathResult([pathHit(fixture.nodes[12], path)])));
  await openGraph(page); await page.locator("#graph-search-form summary").click(); await page.locator("#graph-hops").fill("2"); await page.locator("#graph-retrieval-direction").selectOption("both"); await askGraph(page); await page.getByText("How this passage was found", { exact: true }).click();
  await page.locator(".graph-retrieval").screenshot({ path: "/private/tmp/vectors-retrieval-path-desktop.png" }); expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.setViewportSize({ width: 390, height: 844 }); await page.locator(".graph-retrieval").screenshot({ path: "/private/tmp/vectors-retrieval-path-mobile.png", style: ".topbar { visibility: hidden; }" }); expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});
