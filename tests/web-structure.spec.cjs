"use strict";

const { test, expect } = require("@playwright/test");
const reply = (route, body, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
const deferred = () => { let resolve; const promise = new Promise((done) => { resolve = done; }); return { promise, resolve }; };
const profile = { provider: "openai", model: "text-embedding-3-small", dimensions: 3, context_format_version: 1 };
const field = (name, data_type, nullable = true, unique = false) => ({ name, data_type, nullable, unique });
const collection = (name, document_columns = []) => ({
  config: { name, profile, semantic_neighbors: 3, semantic_threshold: .8 },
  document_columns, revision: 7, document_count: 0, chunk_count: 0, edge_count: 0,
  tables: Object.fromEntries(["config", "documents", "chunks", "edges"].map((kind) => [kind, `graph_${name}_${kind}`])),
});
const typedColumns = [field("category", "TEXT", false), field("year", "INTEGER", false, true), field("rating", "DOUBLE"), field("published", "BOOLEAN", false), field("notes", "TEXT")];

async function workspace(page, collections = [], embeddingOverrides = {}, tables = []) {
  const fixture = { calls: [], collections, revision: 7, tables: new Map(tables), relationships: [] };
  const addCollectionTables = (item) => {
    const names = item.tables || {};
    for (const kind of ["documents", "chunks"]) {
      const name = names[kind] || `graph_${item.config.name}_${kind}`;
      const schema = kind === "documents"
        ? [field("document_id", "TEXT", false, true), field("title", "TEXT"), field("source", "TEXT"), field("text", "TEXT"), field("metadata", "TEXT"), ...(item.document_columns || [])]
        : [field("chunk_id", "TEXT", false, true), field("document_id", "TEXT"), field("text", "TEXT"), field("embedding", "VECTOR(3)")];
      fixture.tables.set(name, { schema, rows: [] });
    }
  };
  collections.forEach(addCollectionTables);
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname.startsWith("/v1/")) fixture.calls.push({ path: url.pathname, method: request.method(), query: Object.fromEntries(url.searchParams), body: request.postData() ? request.postDataJSON() : null });
  });
  await page.route("**/healthz", (route) => reply(route, { status: "ok", version: "0.8.0", storage: "memory" }));
  await page.route("**/v1/**", async (route) => {
    const url = new URL(route.request().url());
    const path = url.pathname;
    const method = route.request().method();
    const body = route.request().postData() ? route.request().postDataJSON() : null;
    if (path === "/v1/settings/embeddings") return reply(route, {
      ...profile, configured: true, persistence: "memory", batch_size: 32, timeout_seconds: 60, max_concurrent_requests: 4,
      providers: [{ id: "openai", label: "OpenAI", configured: true, models: [{ id: profile.model, dimensions: [1536], default_dimensions: 1536 }] }],
      ...embeddingOverrides,
    });
    if (path === "/v1/settings/server") return reply(route, { version: "0.8.0", storage: "memory", authentication: false, compute: {}, limits: {}, capacity: null });
    if (path === "/v1/settings/reranking") return reply(route, { configured: false, model: "rerank-2.5", models: [], timeout_seconds: 60, max_concurrent_requests: 4 });
    if (path === "/v1/tables") return reply(route, { revision: fixture.revision, tables: [...fixture.tables].map(([name, table]) => ({ name, row_count: table.rows.length, column_count: table.schema.length, index_count: 0 })) });
    if (path === "/v1/relationships") {
      if (method === "POST") {
        const { expected_revision, ...definition } = body;
        if (expected_revision !== fixture.revision) return reply(route, { error: { code: "stale_revision", message: "The database changed; refresh before saving." } }, 409);
        const relationship = { ...definition, data_type: "INTEGER", valid: true };
        fixture.relationships.push(relationship); fixture.revision += 1;
        return reply(route, { revision: fixture.revision, relationship });
      }
      return reply(route, { revision: fixture.revision, relationships: fixture.relationships });
    }
    if (path.startsWith("/v1/relationships/") && method === "DELETE") {
      if (body.expected_revision !== fixture.revision) return reply(route, { error: { code: "stale_revision", message: "The database changed; refresh before saving." } }, 409);
      fixture.relationships = fixture.relationships.filter((item) => item.name !== decodeURIComponent(path.split("/").pop())); fixture.revision += 1;
      return reply(route, { revision: fixture.revision, deleted: true });
    }
    if (path === "/v1/admin/tables" && method === "POST") {
      fixture.tables.set(body.name, { schema: body.columns, rows: [] }); fixture.revision += 1;
      return reply(route, { results: [{ type: "command", tag: "CREATE TABLE", rows_affected: 0 }] });
    }
    const insert = path.match(/^\/v1\/tables\/([^/]+)\/rows$/);
    if (insert && method === "POST") {
      const table = fixture.tables.get(decodeURIComponent(insert[1]));
      table.rows.push(...body.rows.map((row) => table.schema.map((column) => row[column.name] ?? null)));
      fixture.revision += 1;
      return reply(route, { results: [{ type: "command", tag: "INSERT", rows_affected: body.rows.length }] });
    }
    const metadata = path.match(/^\/v1\/tables\/([^/]+)\/(schema|indexes)$/);
    if (metadata) return reply(route, metadata[2] === "schema" ? { columns: fixture.tables.get(decodeURIComponent(metadata[1])).schema } : { indexes: [] });
    const browse = path.match(/^\/v1\/admin\/tables\/([^/]+)\/rows$/);
    if (browse) {
      const name = decodeURIComponent(browse[1]); const table = fixture.tables.get(name);
      if (method === "PATCH") {
        if (body.expected_revision !== fixture.revision) return reply(route, { error: { code: "stale_revision", message: "The database changed; refresh before saving." } }, 409);
        const keyIndex = table.schema.findIndex((column) => column.name === body.key.column);
        const row = table.rows.find((values) => values[keyIndex] === body.key.value);
        for (const [column, value] of Object.entries(body.values)) row[table.schema.findIndex((field) => field.name === column)] = value;
        fixture.revision += 1;
        return reply(route, { results: [{ type: "command", tag: "UPDATE", rows_affected: 1 }] });
      }
      return reply(route, { table: name, columns: table.schema.map((column) => column.name), schema: table.schema, rows: table.rows, total_rows: table.rows.length, limit: Number(url.searchParams.get("limit") || 50), offset: 0, revision: fixture.revision });
    }
    if (path === "/v1/graph/collections") {
      if (method === "POST") {
        const added = collection(body.name, body.document_columns || []); fixture.collections.push(added); addCollectionTables(added); fixture.revision += 1;
        return reply(route, added);
      }
      return reply(route, fixture.collections);
    }
    const graph = path.match(/^\/v1\/graph\/collections\/([^/]+)(?:\/(graph|documents))?$/);
    if (graph) {
      const item = fixture.collections.find((item) => item.config.name === decodeURIComponent(graph[1]));
      if (!graph[2]) return reply(route, item);
      if (graph[2] === "graph") return reply(route, { collection: item.config.name, revision: fixture.revision, nodes: [], edges: [], total_nodes: 0, total_edges: 0, offset: 0, limit: 100, truncated: false });
      fixture.revision += 1;
      return reply(route, { document_id: body.id, revision: fixture.revision, chunks: 1, edges_created: 0, replaced: false, unchanged: false, embedding_usage: { total_tokens: 5 } });
    }
    return reply(route, { error: { code: "unexpected_request", message: "Unexpected structure test request" } }, 404);
  });
  await page.goto("/");
  await expect(page.locator("#status-label")).toHaveText("Connected");
  return fixture;
}

async function openGraph(page) {
  await page.locator('.nav-item[data-view="connections"]').click();
  await expect(page.locator("#view-title")).toHaveText("Connections");
}
async function addField(page, prefix, column) {
  if (prefix === "graph-create" && !(await page.locator(`#${prefix}-fields-add`).isVisible())) {
    await page.locator("#graph-create-dialog summary").filter({ hasText: "Document fields" }).click();
  }
  await page.locator(`#${prefix}-fields-add`).click();
  const row = page.locator(`#${prefix}-fields [data-field-row]`).last();
  await row.locator("[data-field-name]").fill(column.name);
  await row.locator("[data-field-type]").selectOption(column.data_type);
  await row.locator("[data-field-required]").setChecked(!column.nullable);
  await row.locator("[data-field-unique]").setChecked(column.unique);
  return row;
}
async function documentDraft(page) {
  await page.locator("#graph-add-open").click();
  await page.locator("#graph-document-id").fill("first-note");
  await page.locator("#graph-document-title").fill("First note");
  await page.locator("#graph-document-text").fill("This is the first document in this collection.");
}
const creates = (fixture) => fixture.calls.filter((call) => call.path === "/v1/graph/collections" && call.method === "POST");
const saves = (fixture) => fixture.calls.filter((call) => call.path.endsWith("/documents") && call.method === "POST");

test("the Data create chooser reuses the typed table builder and its existing workspace", async ({ page }) => {
  const fixture = await workspace(page);
  await page.locator('.nav-item[data-view="data"]').click();
  await page.locator("#admin-new-table").click();
  await expect(page.locator("#data-create-dialog")).toBeVisible();
  await page.locator("#data-create-table").click();
  await page.locator("#admin-create-name").fill("catalog");
  await page.locator("#admin-create-dimensions").fill("3");
  await addField(page, "admin-create", field("year", "INTEGER", false, true));
  await page.locator("#admin-create-submit").click();
  await expect(page.locator("#admin-create-dialog")).not.toBeVisible();
  await expect(page.locator("#admin-table")).toHaveValue("catalog");
  const requests = fixture.calls.filter((call) => call.path === "/v1/admin/tables" && call.method === "POST");
  expect(requests).toHaveLength(1);
  expect(requests[0].body.columns).toEqual(expect.arrayContaining([field("year", "INTEGER", false, true), expect.objectContaining({ name: "embedding", data_type: "VECTOR(3)" })]));
  expect(fixture.calls.filter((call) => call.path === "/v1/sql")).toHaveLength(0);
});

test("a typed collection created from Data opens its first document and preserves scalar types", async ({ page }) => {
  const fixture = await workspace(page);
  await page.locator('.nav-item[data-view="data"]').click();
  await page.locator("#admin-new-table").click();
  await page.locator("#data-create-collection").click();
  await page.locator("#graph-create-name").fill("research");
  for (const column of typedColumns) await addField(page, "graph-create", column);
  await page.locator("#graph-create-submit").click();
  await expect(page.locator("#graph-create-dialog")).not.toBeVisible();
  await expect(page.locator("#view-title")).toHaveText("Connections");
  await expect(page.locator("#graph-collection")).toHaveValue("research");
  await expect(page.locator("#graph-document-panel")).toHaveAttribute("open", "");
  expect(creates(fixture)).toHaveLength(1);
  expect(creates(fixture)[0].body.document_columns).toEqual(typedColumns);
  expect(saves(fixture)).toHaveLength(0);
  await documentDraft(page);
  await page.locator('[data-document-field="category"]').fill("Guide");
  await page.locator('[data-document-field="year"]').fill("0");
  await page.locator('[data-document-field="rating"]').fill("0.25");
  await page.locator('[data-document-field="published"]').selectOption("false");
  await page.locator("#graph-document-save").click();
  await expect(page.locator("#graph-document-status")).toContainText("Saved document");
  expect(saves(fixture)).toHaveLength(1);
  expect(saves(fixture)[0]).toMatchObject({ path: "/v1/graph/collections/research/documents", body: { id: "first-note", metadata: { category: "Guide", year: 0, rating: .25, published: false, notes: null } } });
  expect(fixture.calls.filter((call) => call.path === "/v1/embeddings")).toHaveLength(0);
});

test("nullable fields can explicitly discard a value while required false and zero remain valid", async ({ page }) => {
  const fixture = await workspace(page, [collection("typed", typedColumns)]);
  await openGraph(page); await documentDraft(page);
  await page.locator('[data-document-field="category"]').fill("Guide");
  await page.locator('[data-document-field="year"]').fill("0");
  await page.locator('[data-document-field="published"]').selectOption("false");
  await page.locator('[data-document-field="notes"]').fill("Discard this optional value");
  await page.locator('[data-document-null="notes"]').check();
  await expect(page.locator('[data-document-field="notes"]')).toBeDisabled();
  await page.locator("#graph-document-save").click();
  await expect(page.locator("#graph-document-status")).toContainText("Saved document");
  expect(saves(fixture)).toHaveLength(1);
  expect(saves(fixture)[0].body.metadata).toEqual({ category: "Guide", year: 0, rating: null, published: false, notes: null });
});

test("invalid names, duplicates, and reserved collection fields never reach the create API", async ({ page }) => {
  const fixture = await workspace(page);
  await openGraph(page); await page.locator("#graph-create-open").click();
  await page.locator("#graph-create-name").fill("research");
  const row = await addField(page, "graph-create", field("bad-name", "TEXT"));
  for (const name of ["bad-name", "document_id", "Metadata"]) {
    await row.locator("[data-field-name]").fill(name);
    await page.locator("#graph-create-submit").click();
    await expect(page.locator("#graph-create-dialog")).toBeVisible();
    expect(creates(fixture)).toHaveLength(0);
  }
  await row.locator("[data-field-name]").fill("category");
  const duplicate = await addField(page, "graph-create", field("Category", "TEXT"));
  await page.locator("#graph-create-submit").click();
  await expect(page.locator("#graph-create-status")).toContainText(/duplicate|already|unique/i);
  expect(creates(fixture)).toHaveLength(0);
  await duplicate.locator("[data-field-remove]").click();
  await page.locator("#graph-create-submit").click();
  await expect(page.locator("#graph-create-dialog")).not.toBeVisible();
  expect(creates(fixture)).toHaveLength(1);
  expect(creates(fixture)[0].body.document_columns).toEqual([field("category", "TEXT")]);
});

test("required and numeric document validation happens before a paid document request", async ({ page }) => {
  const fixture = await workspace(page, [collection("typed", typedColumns)]);
  await openGraph(page); await documentDraft(page);
  await page.locator("#graph-document-save").click();
  expect(saves(fixture)).toHaveLength(0);
  await page.locator('[data-document-field="category"]').fill("Guide");
  await page.locator('[data-document-field="published"]').selectOption("false");
  await page.locator('[data-document-field="year"]').fill("1.5");
  await page.locator("#graph-document-save").click();
  expect(saves(fixture)).toHaveLength(0);
  await expect(page.locator('[data-document-field="year"]')).toHaveValue("1.5");
  await page.locator('[data-document-field="year"]').fill("9007199254740992");
  await page.locator("#graph-document-save").click();
  expect(saves(fixture)).toHaveLength(0);
  await page.locator('[data-document-field="year"]').fill("2026");
  await page.locator("#graph-document-save").click();
  await expect(page.locator("#graph-document-status")).toContainText("Saved document");
  expect(saves(fixture)).toHaveLength(1);
});

test("an invalid optional numeric draft is rejected instead of being silently saved as null", async ({ page }) => {
  const fixture = await workspace(page, [collection("typed", [field("rating", "DOUBLE")])]);
  await openGraph(page); await documentDraft(page);
  const rating = page.locator('[data-document-field="rating"]');
  await rating.pressSequentially("-");
  expect(await rating.evaluate((input) => input.validity.badInput)).toBe(true);
  await page.locator("#graph-document-save").click();
  await expect(page.locator("#graph-document-status")).toContainText(/valid|finite|number/i);
  expect(saves(fixture)).toHaveLength(0);
  await page.locator('[data-document-null="rating"]').check();
  await page.locator("#graph-document-save").click();
  await expect(page.locator("#graph-document-status")).toContainText("Saved document");
  expect(saves(fixture)).toHaveLength(1);
  expect(saves(fixture)[0].body.metadata).toEqual({ rating: null });
});

test("legacy collections without typed columns retain the ordinary document flow", async ({ page }) => {
  const legacy = collection("legacy"); delete legacy.document_columns;
  const fixture = await workspace(page, [legacy]);
  await openGraph(page); await documentDraft(page);
  await expect(page.locator("[data-document-field]")).toHaveCount(0);
  await page.locator("#graph-document-save").click();
  await expect(page.locator("#graph-document-status")).toContainText("Saved document");
  expect(saves(fixture)).toHaveLength(1);
  expect(saves(fixture)[0].body.metadata || {}).toEqual({});
});

test("metadata-only saves can reuse existing embeddings when current provider settings differ", async ({ page }) => {
  const fixture = await workspace(page, [collection("typed", [field("year", "INTEGER")])], { configured: false, model: "another-model", dimensions: 4 });
  await page.route("**/collections/typed/documents", (route) => reply(route, {
    document_id: "first-note", revision: 8, chunks: 1, edges_created: 0, replaced: false,
    unchanged: false, embeddings_reused: true, embedding_usage: { total_tokens: 0 },
  }));
  await openGraph(page); await documentDraft(page);
  await page.locator('[data-document-field="year"]').fill("2026");
  await page.locator("#graph-document-save").click();
  await expect(page.locator("#graph-document-status")).toContainText(/fields updated|embeddings.*reused/i);
  expect(saves(fixture)).toHaveLength(1);
  expect(saves(fixture)[0].body.metadata).toEqual({ year: 2026 });
  expect(fixture.calls.filter((call) => call.path === "/v1/embeddings")).toHaveLength(0);
});

test("typed collection handoffs reuse Data and SQL without executing a query", async ({ page }) => {
  const fixture = await workspace(page, [collection("research", [field("year", "INTEGER")])]);
  await openGraph(page); await page.locator("#graph-view-data").click();
  await expect(page.locator("#view-title")).toHaveText("Data");
  await expect(page.locator("#admin-table")).toHaveValue("graph_research_documents");
  await expect.poll(() => fixture.calls.some((call) => call.path === "/v1/admin/tables/graph_research_documents/rows")).toBe(true);
  await openGraph(page); await page.locator("#graph-open-sql").click();
  await expect(page.locator("#view-console")).toBeVisible();
  await expect(page.locator("#sql-editor")).toHaveValue(/SELECT[\s\S]+graph_research_/i);
  expect(fixture.calls.filter((call) => call.path === "/v1/sql")).toHaveLength(0);
  await expect(page.locator("#admin-table")).toHaveCount(1);
  await expect(page.locator("#sql-editor")).toHaveCount(1);
});

test("legacy collection handoffs infer table names when the response omits them", async ({ page }) => {
  const legacy = collection("legacy"); delete legacy.document_columns; delete legacy.tables;
  const fixture = await workspace(page, [legacy]);
  await openGraph(page); await page.locator("#graph-view-data").click();
  await expect(page.locator("#admin-table")).toHaveValue("graph_legacy_documents");
  await openGraph(page); await page.locator("#graph-open-sql").click();
  await expect(page.locator("#sql-editor")).toHaveValue(/graph_legacy_chunks[\s\S]+graph_legacy_documents/);
  expect(fixture.calls.filter((call) => call.path === "/v1/sql")).toHaveLength(0);
});

test("slow collection creation locks its structure and sends exactly one request", async ({ page }) => {
  const fixture = await workspace(page);
  await openGraph(page); await page.locator("#graph-create-open").click();
  await page.locator("#graph-create-name").fill("research");
  const row = await addField(page, "graph-create", field("year", "INTEGER"));
  const entered = deferred(); const release = deferred();
  await page.route("**/v1/graph/collections", async (route) => {
    if (route.request().method() !== "POST") return route.fallback();
    entered.resolve(); await release.promise; return route.fallback();
  });
  await page.locator("#graph-create-submit").click(); await entered.promise;
  await expect(row.locator("[data-field-type]")).toBeDisabled();
  await expect(row.locator("[data-field-required]")).toBeDisabled();
  await expect(page.locator("#graph-create-fields-add")).toBeDisabled();
  await page.locator("#graph-create-form").evaluate((form) => form.requestSubmit());
  release.resolve();
  await expect(page.locator("#graph-create-dialog")).not.toBeVisible();
  expect(creates(fixture)).toHaveLength(1);
});

test("slow document saving freezes typed metadata and cannot be submitted twice", async ({ page }) => {
  const fixture = await workspace(page, [collection("typed", [field("published", "BOOLEAN"), field("year", "INTEGER")])]);
  await openGraph(page); await documentDraft(page);
  await page.locator('[data-document-field="published"]').selectOption("false");
  await page.locator('[data-document-field="year"]').fill("0");
  const entered = deferred(); const release = deferred();
  await page.route("**/collections/typed/documents", async (route) => { entered.resolve(); await release.promise; return route.fallback(); });
  await page.locator("#graph-document-save").click(); await entered.promise;
  await expect(page.locator('[data-document-field="published"]')).toBeDisabled();
  await expect(page.locator('[data-document-null="year"]')).toBeDisabled();
  await expect(page.locator("#graph-collection")).toBeDisabled();
  await page.locator("#graph-document-form").evaluate((form) => form.requestSubmit());
  release.resolve();
  await expect(page.locator("#graph-document-status")).toContainText("Saved document");
  expect(saves(fixture)).toHaveLength(1);
  expect(saves(fixture)[0].body.metadata).toEqual({ published: false, year: 0 });
});

test("switching collections clears typed metadata and a late old graph cannot restore it", async ({ page }) => {
  const fixture = await workspace(page, [collection("alpha", [field("secret", "TEXT")]), collection("beta", [field("year", "INTEGER")])]);
  await openGraph(page); await documentDraft(page);
  await page.locator('[data-document-field="secret"]').fill("Alpha private draft");
  const entered = deferred(); const release = deferred();
  await page.route("**/collections/alpha/graph?*", async (route) => { entered.resolve(); await release.promise; return route.fallback(); });
  await page.locator("#graph-refresh").click(); await entered.promise;
  await page.locator("#graph-collection").selectOption("beta");
  await expect(page.locator('[data-document-field="year"]')).toBeVisible();
  release.resolve();
  await expect(page.locator('[data-document-field="secret"]')).toHaveCount(0);
  await expect(page.locator('[data-document-field="year"]')).toHaveValue("");
  await documentDraft(page);
  await page.locator('[data-document-field="year"]').fill("2026");
  await page.locator("#graph-document-save").click();
  await expect(page.locator("#graph-document-status")).toContainText("Saved document");
  expect(saves(fixture)).toHaveLength(1);
  expect(saves(fixture)[0]).toMatchObject({ path: "/v1/graph/collections/beta/documents", body: { metadata: { year: 2026 } } });
});

test("a late collection catalog cannot replace a newer selection or its typed draft", async ({ page }) => {
  const fixture = await workspace(page, [collection("alpha", [field("secret", "TEXT")]), collection("beta", [field("year", "INTEGER")])]);
  await openGraph(page); await documentDraft(page);
  await page.locator('[data-document-field="secret"]').fill("Old collection draft");
  const entered = deferred(); const release = deferred();
  await page.route("**/v1/graph/collections", async (route) => {
    if (route.request().method() !== "GET") return route.fallback();
    entered.resolve(); await release.promise; return reply(route, [fixture.collections[0]]);
  });
  await page.locator("#graph-refresh").click(); await entered.promise;
  await page.locator("#graph-collection").selectOption("beta");
  await page.locator('[data-document-field="year"]').fill("2026");
  const response = page.waitForResponse((response) => new URL(response.url()).pathname === "/v1/graph/collections");
  release.resolve(); await (await response).finished();
  await expect(page.locator("#graph-collection")).toHaveValue("beta");
  await expect(page.locator('[data-document-field="year"]')).toHaveValue("2026");
  await expect(page.locator('[data-document-field="secret"]')).toHaveCount(0);
  await documentDraft(page);
  await page.locator("#graph-document-save").click();
  await expect(page.locator("#graph-document-status")).toContainText("Saved document");
  expect(saves(fixture)).toHaveLength(1);
  expect(saves(fixture)[0]).toMatchObject({ path: "/v1/graph/collections/beta/documents", body: { metadata: { year: 2026 } } });
});

test("changing the connection clears typed drafts and ignores a late save response", async ({ page }) => {
  const fixture = await workspace(page, [collection("typed", [field("secret", "TEXT")])]);
  await openGraph(page); await documentDraft(page);
  await page.locator('[data-document-field="secret"]').fill("Old connection draft");
  const entered = deferred(); const release = deferred();
  await page.route("**/collections/typed/documents", async (route) => { entered.resolve(); await release.promise; return reply(route, { revision: 8, chunks: 1, edges_created: 0, embedding_usage: { total_tokens: 5 } }); });
  await page.locator("#graph-document-save").click(); await entered.promise;
  await page.locator("#open-token").click(); await page.locator("#token-input").fill("new-structure-session"); await page.locator("#save-token").click();
  release.resolve();
  await expect(page.locator("#status-label")).toHaveText("Connected");
  await expect(page.locator('[data-document-field="secret"]')).toHaveValue("");
  await expect(page.locator("#graph-document-text")).toHaveValue("");
  await expect(page.locator("#graph-document-status")).not.toContainText("Saved document");
  expect(saves(fixture)).toHaveLength(1);
});

const relationshipCollections = () => [collection("research", [field("year", "INTEGER")]), collection("reference", [field("year", "INTEGER"), field("active", "BOOLEAN")])];
const relationshipDefinition = { name: "papers_by_year", source_table: "graph_research_documents", source_column: "year", target_table: "graph_reference_documents", target_column: "year" };
async function relationshipDraft(page) {
  await page.locator('.nav-item[data-view="data"]').click();
  await page.locator("#relationship-new").click();
  await page.locator("#relationship-name").fill("papers_by_year");
  await page.locator("#relationship-source-table").selectOption("graph_research_documents");
  await page.locator("#relationship-source-column").selectOption("year");
  await page.locator("#relationship-target-table").selectOption("graph_reference_documents");
  await page.locator("#relationship-target-column").selectOption("year");
}

const recordLink = { name: "article_category", source_table: "articles", source_column: "category_id", target_table: "categories", target_column: "id" };
const recordLinkTables = (rows = []) => [
  ["articles", { schema: [field("id", "INTEGER", false, true), field("category_id", "INTEGER")], rows }],
  ["categories", { schema: [field("id", "INTEGER", false, true)], rows: [[10], [20]] }],
];
async function recordLinkDraft(page) {
  await page.locator("#relationship-new").click();
  await page.locator("#relationship-name").fill(recordLink.name);
  await page.locator("#relationship-source-table").selectOption(recordLink.source_table);
  await page.locator("#relationship-source-column").selectOption(recordLink.source_column);
  await page.locator("#relationship-target-table").selectOption(recordLink.target_table);
  await page.locator("#relationship-target-column").selectOption(recordLink.target_column);
}

test("a row insert refreshes relationship revisions before the next link is saved", async ({ page }) => {
  const fixture = await workspace(page, [], {}, recordLinkTables());
  await page.locator('.nav-item[data-view="data"]').click();
  await expect(page.locator("#relationship-new")).toBeEnabled();
  await page.locator("#admin-table").selectOption("articles");
  await page.locator("#admin-row-json").fill(JSON.stringify({ id: 1, category_id: 10 }));

  const refreshEntered = deferred(); const releaseRefresh = deferred();
  await page.route("**/v1/relationships", async (route) => {
    if (route.request().method() === "GET" && fixture.revision === 8) {
      refreshEntered.resolve(); await releaseRefresh.promise;
    }
    return route.fallback();
  });
  await page.locator("#admin-add-row").click();
  await expect(page.locator("#admin-edit-status")).toContainText("Row inserted");
  await refreshEntered.promise;
  await expect(page.locator("#relationship-new")).toBeDisabled();
  releaseRefresh.resolve();
  await expect(page.locator("#relationship-new")).toBeEnabled();

  await recordLinkDraft(page);
  await page.locator("#relationship-save").click();
  await expect(page.locator("#relationship-dialog")).not.toBeVisible();
  await expect(page.locator('[data-relationship-name="article_category"]')).toBeVisible();
  expect(fixture.calls.filter((call) => call.path === "/v1/tables/articles/rows" && call.method === "POST")).toHaveLength(1);
  const links = fixture.calls.filter((call) => call.path === "/v1/relationships" && call.method === "POST");
  expect(links).toHaveLength(1);
  expect(links[0].body).toEqual({ ...recordLink, expected_revision: 8 });
});

for (const action of ["create", "remove"]) {
  test(`a relationship ${action} preserves a row draft and advances its matching revision`, async ({ page }) => {
    const fixture = await workspace(page, [], {}, recordLinkTables([[1, 10]]));
    if (action === "remove") fixture.relationships = [{ ...recordLink, data_type: "INTEGER", valid: true }];
    await page.locator('.nav-item[data-view="data"]').click();
    await page.locator("#admin-table").selectOption("articles");
    await page.getByRole("button", { name: "Edit row 1", exact: true }).click();
    const draft = '{"id":1,"category_id":20}';
    await page.locator("#admin-row-json").fill(draft);
    const pageReads = fixture.calls.filter((call) => call.path === "/v1/admin/tables/articles/rows" && call.method === "GET").length;
    if (action === "create") {
      await recordLinkDraft(page);
      await page.locator("#relationship-save").click();
      await expect(page.locator("#relationship-dialog")).not.toBeVisible();
      await expect(page.locator('[data-relationship-name="article_category"]')).toBeVisible();
    } else {
      await page.locator('[data-relationship-name="article_category"] [data-relationship-remove]').click();
      await expect(page.locator('[data-relationship-name="article_category"]')).toHaveCount(0);
    }
    await expect(page.locator("#relationship-new")).toBeEnabled();
    await expect(page.locator("#admin-row-json")).toHaveValue(draft);
    await expect(page.locator("#admin-status")).toHaveText("articles · revision 8");
    expect(fixture.calls.filter((call) => call.path === "/v1/admin/tables/articles/rows" && call.method === "GET")).toHaveLength(pageReads);
    await page.locator("#admin-save-row").click();
    await expect(page.locator("#admin-rows tbody tr").first().locator("td").nth(2)).toHaveText("20");
    const writes = fixture.calls.filter((call) => call.path === "/v1/admin/tables/articles/rows" && call.method === "PATCH");
    expect(writes).toHaveLength(1);
    expect(writes[0].body).toEqual({ expected_revision: 8, key: { column: "id", value: 1 }, values: { category_id: 20 } });
    expect(fixture.tables.get("articles").rows).toEqual([[1, 20]]);
  });
}

test("a relationship write cannot promote a row draft already stale from another write", async ({ page }) => {
  const fixture = await workspace(page, [], {}, recordLinkTables([[1, 10]]));
  await page.locator('.nav-item[data-view="data"]').click();
  await page.locator("#admin-table").selectOption("articles");
  await page.getByRole("button", { name: "Edit row 1", exact: true }).click();
  const draft = '{"id":1,"category_id":20}';
  await page.locator("#admin-row-json").fill(draft);
  fixture.tables.get("articles").rows[0][1] = 30;
  fixture.revision = 8;
  await page.locator("#relationships-refresh").click();
  await recordLinkDraft(page);
  await page.locator("#relationship-save").click();
  await expect(page.locator("#relationship-dialog")).not.toBeVisible();
  await expect(page.locator('[data-relationship-name="article_category"]')).toBeVisible();
  await expect(page.locator("#admin-row-json")).toHaveValue(draft);
  await page.locator("#admin-save-row").click();
  await expect(page.locator("#admin-edit-status")).toContainText("data changed");
  await expect(page.locator("#admin-row-json")).toHaveValue(draft);
  const writes = fixture.calls.filter((call) => call.path === "/v1/admin/tables/articles/rows" && call.method === "PATCH");
  expect(writes).toHaveLength(1);
  expect(writes[0].body.expected_revision).toBe(7);
  expect(fixture.tables.get("articles").rows).toEqual([[1, 30]]);
});

test("saved scalar relationships use compatible fields, one revision-protected write, and SQL handoff", async ({ page }) => {
  const fixture = await workspace(page, relationshipCollections());
  await relationshipDraft(page);
  expect(await page.locator("#relationship-target-column option").evaluateAll((options) => options.map((option) => option.value).filter(Boolean))).toEqual(["year"]);
  const entered = deferred(); const release = deferred();
  await page.route("**/v1/relationships", async (route) => {
    if (route.request().method() !== "POST") return route.fallback();
    entered.resolve(); await release.promise; return route.fallback();
  });
  await page.locator("#relationship-save").click(); await entered.promise;
  await expect(page.locator("#relationship-source-table")).toBeDisabled();
  await page.locator("#relationship-form").evaluate((form) => form.requestSubmit());
  release.resolve();
  await expect(page.locator("#relationship-dialog")).not.toBeVisible();
  const card = page.locator('[data-relationship-name="papers_by_year"]');
  await expect(card).toBeVisible();
  const posts = fixture.calls.filter((call) => call.path === "/v1/relationships" && call.method === "POST");
  expect(posts).toHaveLength(1);
  expect(posts[0].body).toEqual({ ...relationshipDefinition, expected_revision: 7 });
  await card.locator("[data-relationship-sql]").click();
  await expect(page.locator("#view-console")).toBeVisible();
  await expect(page.locator("#sql-editor")).toHaveValue(/LEFT JOIN[\s\S]+graph_reference_documents[\s\S]+year/i);
  expect(fixture.calls.filter((call) => call.path === "/v1/sql")).toHaveLength(0);
});

test("removing a saved relationship deletes only its definition using the displayed revision", async ({ page }) => {
  const fixture = await workspace(page, relationshipCollections());
  fixture.relationships = [{ ...relationshipDefinition, data_type: "INTEGER", valid: true }];
  await page.locator('.nav-item[data-view="data"]').click();
  await page.locator('[data-relationship-name="papers_by_year"] [data-relationship-remove]').click();
  await expect(page.locator('[data-relationship-name="papers_by_year"]')).toHaveCount(0);
  const deletes = fixture.calls.filter((call) => call.method === "DELETE");
  expect(deletes).toHaveLength(1);
  expect(deletes[0]).toMatchObject({ path: "/v1/relationships/papers_by_year", body: { expected_revision: 7 } });
  expect([...fixture.tables.keys()]).toHaveLength(4);
  expect(fixture.calls.filter((call) => call.path === "/v1/sql" || call.path.includes("/admin/tables/") && call.method === "DELETE")).toHaveLength(0);
});

test("a stale relationship write retains the draft until an explicit refresh and retry", async ({ page }) => {
  const fixture = await workspace(page, relationshipCollections());
  let reject = true;
  await page.route("**/v1/relationships", (route) => {
    if (route.request().method() === "POST" && reject) {
      reject = false; fixture.revision = 8;
      return reply(route, { error: { code: "stale_revision", message: "The catalog changed; refresh before saving" } }, 409);
    }
    return route.fallback();
  });
  await relationshipDraft(page); await page.locator("#relationship-save").click();
  await expect(page.locator("#relationship-status")).toContainText(/changed|refresh/i);
  await expect(page.locator("#relationship-dialog")).toBeVisible();
  await expect(page.locator("#relationship-name")).toHaveValue("papers_by_year");
  await expect(page.locator("#relationship-source-column")).toHaveValue("year");
  await expect(page.locator("#relationship-target-column")).toHaveValue("year");
  expect(fixture.calls.filter((call) => call.path === "/v1/relationships" && call.method === "POST")).toHaveLength(1);
  await page.locator("#relationship-dialog-refresh").click();
  await expect(page.locator("#relationship-save")).toBeEnabled();
  await expect(page.locator("#relationship-name")).toHaveValue("papers_by_year");
  await expect(page.locator("#relationship-source-column")).toHaveValue("year");
  await expect(page.locator("#relationship-target-column")).toHaveValue("year");
  await page.locator("#relationship-save").click();
  await expect(page.locator("#relationship-dialog")).not.toBeVisible();
  const posts = fixture.calls.filter((call) => call.path === "/v1/relationships" && call.method === "POST");
  expect(posts).toHaveLength(2);
  expect(posts.map((call) => call.body.expected_revision)).toEqual([7, 8]);
});

test("relationship duplicate names and endpoints are blocked before any write", async ({ page }) => {
  const fixture = await workspace(page, relationshipCollections());
  fixture.relationships = [{ ...relationshipDefinition, data_type: "INTEGER", valid: true }];
  await relationshipDraft(page);
  await expect(page.locator("#relationship-save")).toBeDisabled();
  await expect(page.locator("#relationship-status")).toContainText(/already exists/i);
  await page.locator("#relationship-name").fill("another_name");
  await expect(page.locator("#relationship-save")).toBeDisabled();
  await page.locator("#relationship-source-column").selectOption("title");
  await page.locator("#relationship-target-column").selectOption("title");
  await page.locator("#relationship-name").fill("PAPERS_BY_YEAR");
  await expect(page.locator("#relationship-save")).toBeDisabled();
  expect(fixture.calls.filter((call) => call.path === "/v1/relationships" && call.method === "POST")).toHaveLength(0);
  await page.locator("#relationship-name").fill("bad-name");
  await page.locator("#relationship-save").click();
  await expect(page.locator("#relationship-status")).toContainText(/relationship name/i);
  expect(fixture.calls.filter((call) => call.path === "/v1/relationships" && call.method === "POST")).toHaveLength(0);
});

test("late relationship schema loading cannot replace fields from a newer source table", async ({ page }) => {
  await workspace(page, relationshipCollections());
  const entered = deferred(); const release = deferred();
  await page.route("**/v1/tables/graph_research_chunks/schema", async (route) => { entered.resolve(); await release.promise; return route.fallback(); });
  await page.locator('.nav-item[data-view="data"]').click();
  await page.locator("#relationship-new").click();
  await page.locator("#relationship-source-table").selectOption("graph_research_chunks"); await entered.promise;
  await page.locator("#relationship-source-table").selectOption("graph_reference_documents");
  await page.locator("#relationship-source-column").selectOption("year");
  const response = page.waitForResponse((response) => new URL(response.url()).pathname === "/v1/tables/graph_research_chunks/schema");
  release.resolve(); await (await response).finished();
  await expect(page.locator("#relationship-source-table")).toHaveValue("graph_reference_documents");
  await expect(page.locator("#relationship-source-column")).toHaveValue("year");
  await expect(page.locator('#relationship-source-column option[value="embedding"]')).toHaveCount(0);
  await expect(page.locator('#relationship-source-column option[value="chunk_id"]')).toHaveCount(0);
});

test("an older server without relationship APIs leaves Data browsing and creation usable", async ({ page }) => {
  await workspace(page, relationshipCollections());
  await page.route("**/v1/relationships", (route) => reply(route, { error: { code: "not_found", message: "Not found" } }, 404));
  await page.locator('.nav-item[data-view="data"]').click();
  await expect(page.locator("#relationships-status")).toContainText(/unavailable|update|support/i);
  await expect(page.locator("#relationship-new")).toBeDisabled();
  await page.locator("#admin-table").selectOption("graph_research_documents");
  await expect(page.locator("#admin-tools")).toBeVisible();
  await page.locator("#admin-new-table").click();
  await expect(page.locator("#data-create-dialog")).toBeVisible();
});

test("a failed relationship refresh preserves displayed links and prevents stale mutations", async ({ page }) => {
  const fixture = await workspace(page, relationshipCollections());
  fixture.relationships = [{ ...relationshipDefinition, data_type: "INTEGER", valid: true }];
  await page.locator('.nav-item[data-view="data"]').click();
  const card = page.locator('[data-relationship-name="papers_by_year"]');
  await expect(card).toBeVisible();
  await page.route("**/v1/relationships", (route) => reply(route, { error: { code: "overloaded", message: "Link catalog temporarily unavailable" } }, 503));
  await page.locator("#relationships-refresh").click();
  await expect(page.locator("#relationships-status")).toContainText("temporarily unavailable");
  await expect(card).toBeVisible();
  await expect(page.locator("#relationship-new")).toBeDisabled();
  await expect(card.locator("[data-relationship-remove]")).toBeDisabled();
  expect(fixture.calls.filter((call) => call.path.startsWith("/v1/relationships") && call.method !== "GET")).toHaveLength(0);
});

test("a new connection reloads relationships and ignores a late unauthorized catalog response", async ({ page }) => {
  const fixture = await workspace(page, relationshipCollections());
  fixture.relationships = [{ ...relationshipDefinition, name: "old_private_link", data_type: "INTEGER", valid: true }];
  await page.locator('.nav-item[data-view="data"]').click();
  await expect(page.locator('[data-relationship-name="old_private_link"]')).toBeVisible();
  const entered = deferred(); const release = deferred(); const finished = deferred();
  await page.route("**/v1/relationships", async (route) => {
    if (route.request().headers().authorization === "Bearer new-link-session") {
      return reply(route, { revision: 8, relationships: [{ ...relationshipDefinition, name: "new_session_link", data_type: "INTEGER", valid: true }] });
    }
    entered.resolve(); await release.promise;
    try { await reply(route, { error: { code: "unauthorized", message: "Old token expired" } }, 401); }
    finally { finished.resolve(); }
  });
  await page.locator("#relationships-refresh").click(); await entered.promise;
  await page.locator("#open-token").click(); await page.locator("#token-input").fill("new-link-session"); await page.locator("#save-token").click();
  await expect(page.locator('[data-relationship-name="new_session_link"]')).toBeVisible();
  release.resolve(); await finished.promise;
  await expect(page.locator('[data-relationship-name="old_private_link"]')).toHaveCount(0);
  await expect(page.locator("#token-dialog")).not.toBeVisible();
  await expect(page.locator("#relationship-new")).toBeEnabled();
  expect(fixture.calls.filter((call) => call.path.startsWith("/v1/relationships") && call.method !== "GET")).toHaveLength(0);
});
