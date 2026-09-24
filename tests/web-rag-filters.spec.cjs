"use strict";

const { test, expect } = require("@playwright/test");
const reply = (route, body, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
const deferred = () => { let resolve; const promise = new Promise((done) => { resolve = done; }); return { promise, resolve }; };
const profile = { provider: "openai", model: "text-embedding-3-small", dimensions: 3, context_format_version: 1 };
const field = (name, data_type) => ({ name, data_type, nullable: true, unique: false });
const columns = [field("category", "TEXT"), field("year", "INTEGER"), field("score", "DOUBLE"), field("published", "BOOLEAN")];
const collection = (name, document_columns = []) => ({ config: { name, profile }, document_columns, revision: 7, document_count: 0, chunk_count: 0, edge_count: 0, tables: {} });
const result = (text = null) => ({ hits: text ? [{ chunk_id: "first", document_id: "document", title: text, source: "", text, metadata: {}, ordinal: 0, start_byte: 0, end_byte: text.length, depth: 0, seed: true }] : [], reranking: { method: "local" }, context_bytes: text?.length || 0, candidate_count: text ? 1 : 0, truncated: false });

async function workspace(page, collections = [collection("knowledge", columns), collection("archive", [field("region", "TEXT")])]) {
  const fixture = { calls: [], collections };
  page.on("request", (request) => {
    const path = new URL(request.url()).pathname;
    if (path.startsWith("/v1/")) fixture.calls.push({ path, method: request.method(), body: request.postData() ? request.postDataJSON() : null });
  });
  await page.route("**/healthz", (route) => reply(route, { status: "ok", version: "0.8.0", storage: "memory" }));
  await page.route("**/v1/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/v1/tables") return reply(route, { revision: 7, tables: [] });
    if (path === "/v1/settings/embeddings") return reply(route, { ...profile, configured: true, persistence: "memory", batch_size: 32, timeout_seconds: 60, max_concurrent_requests: 4, providers: [{ id: "openai", label: "OpenAI", configured: true, models: [{ id: profile.model, dimensions: [3], default_dimensions: 3 }] }] });
    if (path === "/v1/settings/reranking") return reply(route, { configured: false, model: "rerank-2.5", models: [], timeout_seconds: 60, max_concurrent_requests: 4 });
    if (path === "/v1/settings/server") return reply(route, { version: "0.8.0", storage: "memory", authentication: false, compute: {}, limits: {}, capacity: null });
    if (path === "/v1/graph/collections") return reply(route, fixture.collections);
    if (path.endsWith("/graph")) return reply(route, { collection: path.split("/")[4], revision: 7, nodes: [], edges: [], total_nodes: 0, total_edges: 0, offset: 0, limit: 100, truncated: false });
    if (path.endsWith("/retrieve")) return reply(route, result());
    return reply(route, { error: { code: "not_found", message: "Unexpected filter test request" } }, 404);
  });
  await page.goto("/"); await expect(page.locator("#status-label")).toHaveText("Connected");
  if (await page.locator("#mobile-menu").isVisible()) await page.locator("#mobile-menu").click();
  await page.locator('.nav-item[data-view="connections"]').click();
  await expect(page.locator("#graph-collection")).toHaveValue(collections[0]?.config.name || "");
  await expect(page.locator("#graph-status")).toContainText(collections.length ? "0 passages" : "Create");
  return fixture;
}
const retrievals = (fixture) => fixture.calls.filter((call) => call.path.endsWith("/retrieve"));
async function addFilter(page, column, operator = "eq", value = "") {
  if (!(await page.locator("#graph-filter-add").isVisible())) await page.locator("#graph-filters-panel > summary").click();
  await page.locator("#graph-filter-add").click();
  const row = page.locator("[data-graph-filter-row]").last();
  await row.locator("[data-filter-column]").selectOption(column);
  await row.locator("[data-filter-operator]").selectOption(operator);
  if (!["is_null", "is_not_null"].includes(operator)) {
    if (await row.locator("[data-filter-value]").evaluate((element) => element.tagName === "SELECT")) await row.locator("[data-filter-value]").selectOption(value);
    else await row.locator("[data-filter-value]").fill(value);
  }
  return row;
}
async function retrieve(page) { await page.locator("#graph-question").fill("What changed?"); await page.locator("#graph-search-submit").click(); }

test("legacy collections keep the unfiltered request body and expose built-in document fields", async ({ page }) => {
  const fixture = await workspace(page, [collection("legacy")]);
  await expect(page.locator("#graph-filters-panel")).not.toHaveAttribute("open", "");
  await retrieve(page); await expect.poll(() => retrievals(fixture).length).toBe(1);
  expect(retrievals(fixture)[0].body).not.toHaveProperty("document_filters");
  await addFilter(page, "document_id", "eq", "guide");
  await expect(page.locator("[data-filter-column] option")).toHaveText(["Choose a field", "document_id · text", "title · text", "source · text"]);
});

test("new filters prompt for a field and keep keyboard focus when its controls change", async ({ page }) => {
  await workspace(page);
  await page.locator("#graph-filters-panel > summary").click(); await page.locator("#graph-filter-add").click();
  await expect(page.locator("#graph-filter-summary")).toHaveText("Choose a document field for each filter before retrieving context.");
  await expect(page.locator("[data-filter-column]")).toBeFocused();
  await page.locator("[data-filter-column]").selectOption("published");
  await expect(page.locator("[data-filter-column]")).toBeFocused();
  await page.keyboard.press("Tab"); await expect(page.locator("[data-filter-operator]")).toBeFocused();
  await page.keyboard.press("Tab"); await expect(page.locator("[data-filter-value]")).toBeFocused();
  await page.locator("#graph-filter-add").click();
  await page.locator("[data-filter-column]").nth(1).selectOption("year");
  await expect(page.locator("[data-filter-column]").nth(1)).toBeFocused();
});

test("typed AND filters preserve zero, false, null, exact text, and the scope of returned context", async ({ page }) => {
  const fixture = await workspace(page);
  await addFilter(page, "category", "eq", "<img src=x onerror=alert(1)>");
  await addFilter(page, "year", "gte", "0");
  await addFilter(page, "score", "lt", "0.25");
  await addFilter(page, "published", "eq", "false");
  const missing = await addFilter(page, "source", "is_null");
  await expect(missing.locator("[data-filter-value]")).toBeDisabled();
  await page.locator("#graph-filters-panel > summary").click();
  await expect(page.locator("#graph-filter-summary")).toContainText("published equals false AND source is empty");
  await retrieve(page); await expect(page.locator("#graph-search-status")).toContainText("0 passages retrieved");
  expect(retrievals(fixture)[0].body.document_filters).toEqual([
    { column: "category", operator: "eq", value: "<img src=x onerror=alert(1)>" },
    { column: "year", operator: "gte", value: 0 }, { column: "score", operator: "lt", value: .25 },
    { column: "published", operator: "eq", value: false }, { column: "source", operator: "eq", value: null },
  ]);
  await expect(page.locator(".graph-applied-filters")).toContainText("Unmatched documents are excluded");
  await expect(page.locator("#graph-search-results")).toContainText("No passages fit these document filters");
  await expect(page.locator("#graph-filter-summary img, #graph-search-results img")).toHaveCount(0);
});

test("null conditions ignore value drafts while literal null and empty text remain exact strings", async ({ page }) => {
  const fixture = await workspace(page);
  const row = await addFilter(page, "year", "eq", "invalid number");
  await row.locator("[data-filter-operator]").selectOption("is_not_null");
  await addFilter(page, "title", "eq", "null");
  await addFilter(page, "source", "eq", "");
  await retrieve(page); await expect.poll(() => retrievals(fixture).length).toBe(1);
  expect(retrievals(fixture)[0].body.document_filters).toEqual([{ column: "year", operator: "ne", value: null }, { column: "title", operator: "eq", value: "null" }, { column: "source", operator: "eq", value: "" }]);
});

for (const [column, value, message] of [["year", "9007199254740993", "safe whole number"], ["year", "9007199254740991.1", "safe whole number"], ["year", "1.5", "safe whole number"], ["year", "0x10", "must be numeric"], ["score", "1e999", "must be numeric"], ["score", "", "must be numeric"]]) {
  test(`invalid ${column} value ${value || "blank"} blocks retrieval without discarding the draft`, async ({ page }) => {
    const fixture = await workspace(page); const row = await addFilter(page, column, "eq", value);
    await retrieve(page); await expect(page.locator("#graph-search-status")).toContainText(message);
    expect(retrievals(fixture)).toHaveLength(0); await expect(row.locator("[data-filter-value]")).toHaveValue(value);
    await expect(page.locator("#graph-search-submit")).toBeEnabled();
  });
}

test("boolean conditions require a choice and do not offer numeric comparisons", async ({ page }) => {
  const fixture = await workspace(page); const row = await addFilter(page, "published");
  await expect(row.locator("[data-filter-operator] option")).toHaveText(["Equals", "Does not equal", "Is empty (null)", "Has a value"]);
  await retrieve(page); await expect(page.locator("#graph-search-status")).toContainText("true or false");
  expect(retrievals(fixture)).toHaveLength(0);
});

test("collection switches retain separate drafts and reject a late filtered response", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await page.route("**/collections/knowledge/retrieve", async (route) => { entered = true; await gate.promise; await reply(route, result("Obsolete filtered context")); });
  await addFilter(page, "category", "eq", "Guide"); await retrieve(page); await expect.poll(() => entered).toBe(true);
  await page.locator("#graph-collection").selectOption("archive");
  await expect(page.locator("[data-graph-filter-row]")).toHaveCount(0);
  await addFilter(page, "region", "eq", "Europe"); await retrieve(page);
  await expect(page.locator("#graph-search-status")).toContainText("0 passages retrieved"); gate.resolve();
  await expect(page.locator("#graph-search-results")).not.toContainText("Obsolete filtered context");
  expect(retrievals(fixture).at(-1)).toMatchObject({ path: "/v1/graph/collections/archive/retrieve", body: { document_filters: [{ column: "region", operator: "eq", value: "Europe" }] } });
  await page.locator("#graph-collection").selectOption("knowledge");
  await expect(page.locator("[data-filter-column]")).toHaveValue("category"); await expect(page.locator("[data-filter-value]")).toHaveValue("Guide");
  await expect(page.locator("#graph-search-results")).toHaveAttribute("aria-busy", "false");
});

test("catalog refresh preserves drafts but blocks removed fields and changed types", async ({ page }) => {
  const fixture = await workspace(page); await addFilter(page, "year", "gte", "2024");
  fixture.collections[0].document_columns = [field("year", "TEXT")];
  await page.locator("#graph-refresh").click(); await expect(page.locator("#graph-filter-summary")).toContainText("changed or is unavailable");
  await retrieve(page); expect(retrievals(fixture)).toHaveLength(0);
  await expect(page.locator("[data-filter-value]")).toHaveValue("2024");
  fixture.collections[0].document_columns = [];
  await page.locator("#graph-refresh").click(); await expect(page.locator("[data-filter-column] option:checked")).toHaveText("year · unavailable");
  await retrieve(page); expect(retrievals(fixture)).toHaveLength(0);
  await page.locator("#graph-filter-clear").click(); await retrieve(page); await expect.poll(() => retrievals(fixture).length).toBe(1);
  expect(retrievals(fixture)[0].body).not.toHaveProperty("document_filters");
});

test("editing a filter clears context produced by the previous document scope", async ({ page }) => {
  const fixture = await workspace(page);
  await page.route("**/retrieve", (route) => reply(route, result("Guide-only context")));
  const row = await addFilter(page, "category", "eq", "Guide"); await retrieve(page);
  await expect(page.locator(".graph-hit")).toContainText("Guide-only context");
  await expect(page.locator(".graph-applied-filters")).toContainText('category equals "Guide"');
  await row.locator("[data-filter-value]").fill("Reference");
  await expect(page.locator(".graph-hit")).toHaveCount(0);
  await expect(page.locator("#graph-search-results")).toContainText("Document filters changed");
  expect(retrievals(fixture)).toHaveLength(1);
});

test("schema refresh invalidates in-flight retrieval without replacing the filter draft", async ({ page }) => {
  const fixture = await workspace(page); const gate = deferred(); let entered = false;
  await page.route("**/retrieve", async (route) => { entered = true; await gate.promise; await reply(route, result("Old-schema context")); });
  await addFilter(page, "year", "eq", "0"); await retrieve(page); await expect.poll(() => entered).toBe(true);
  fixture.collections[0].document_columns = [field("year", "TEXT")];
  await page.locator("#graph-refresh").click(); await expect(page.locator("#graph-filter-summary")).toContainText("changed or is unavailable");
  const response = page.waitForResponse((response) => response.url().endsWith("/retrieve")); gate.resolve(); await response;
  await expect(page.locator("#graph-search-results")).toContainText("Document filters changed");
  await expect(page.locator(".graph-hit")).toHaveCount(0); await expect(page.locator("[data-filter-value]")).toHaveValue("0");
  await expect(page.locator("#graph-search-submit")).toBeEnabled();
});

test("token changes clear every collection's filter drafts and discard a late forbidden response", async ({ page }) => {
  await workspace(page); const gate = deferred(); let entered = false;
  await addFilter(page, "category", "eq", "private category");
  await page.locator("#graph-collection").selectOption("archive"); await addFilter(page, "region", "eq", "private region");
  await page.route("**/retrieve", async (route) => { entered = true; await gate.promise; await reply(route, { error: { code: "forbidden", message: "Old private filter error" } }, 403); });
  await retrieve(page); await expect.poll(() => entered).toBe(true);
  await page.locator("#open-token").click(); await page.locator("#token-input").fill("replacement-token"); await page.locator("#save-token").click(); gate.resolve();
  await expect(page.locator("#graph-collection")).toHaveValue("knowledge"); await expect(page.locator("[data-graph-filter-row]")).toHaveCount(0);
  await page.locator("#graph-collection").selectOption("archive"); await expect(page.locator("[data-graph-filter-row]")).toHaveCount(0);
  await expect(page.locator("#graph-search-status")).not.toContainText("Old private filter error");
  expect(await page.evaluate(() => JSON.stringify({ ...localStorage, ...sessionStorage }))).not.toContain("private");
  await expect(page.locator("#graph-search-submit")).toBeEnabled();
});

for (const status of [400, 403]) {
  test(`${status} filter errors display as text and preserve editable conditions`, async ({ page }) => {
    const fixture = await workspace(page); const message = "<img src=x onerror=alert(1)> rejected filter";
    await page.route("**/retrieve", (route) => reply(route, { error: { code: "invalid_filter", message } }, status));
    await addFilter(page, "year", "eq", "0"); await retrieve(page);
    await expect(page.locator("#graph-search-status")).toHaveText(message);
    await expect(page.locator("#graph-search-status img")).toHaveCount(0); await expect(page.locator("[data-filter-value]")).toHaveValue("0");
    await expect(page.locator("[data-filter-value]")).toBeEnabled(); expect(retrievals(fixture)).toHaveLength(1);
  });
}

test("filter controls are bounded and usable on mobile without horizontal overflow", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 }); await workspace(page);
  await addFilter(page, "category", "eq", "Guide");
  for (let index = 1; index < 32; index += 1) await page.locator("#graph-filter-add").click();
  await expect(page.locator("[data-graph-filter-row]")).toHaveCount(32); await expect(page.locator("#graph-filter-add")).toBeDisabled();
  await expect(page.locator("#graph-filters-count")).toHaveText(" · 32");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.getByRole("button", { name: "Remove document filter 32", exact: true }).click(); await expect(page.locator("#graph-filter-add")).toBeEnabled();
  await page.locator("#graph-filter-clear").click(); await expect(page.locator("[data-graph-filter-row]")).toHaveCount(0);
});
