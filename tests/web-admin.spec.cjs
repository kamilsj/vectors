"use strict";

const { test, expect } = require("@playwright/test");

const column = (name, data_type, unique = false) => ({ name, data_type, nullable: false, unique });
const documentSchema = [column("id", "INTEGER", true), column("title", "TEXT"), column("content", "TEXT"), column("embedding", "VECTOR(3)")];
const defaultSettings = () => ({
  provider: "openai", model: "text-embedding-3-small", dimensions: 3,
  batch_size: 32, timeout_seconds: 60, max_concurrent_requests: 4,
  configured: true, persistence: "memory",
  providers: [
    { id: "openai", label: "OpenAI", configured: true, models: [{ id: "text-embedding-3-small", dimensions: [1536], default_dimensions: 1536 }] },
    { id: "voyage", label: "Voyage AI", configured: false, models: [{ id: "voyage-4", dimensions: [256, 512, 1024, 2048], default_dimensions: 1024 }] },
  ],
});

function deferred() {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
}

const reply = (route, body, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
const command = (tag, rows_affected = 1) => ({ results: [{ type: "command", tag, rows_affected }] });

async function workspace(page, { count = 3, dimensions = 3 } = {}) {
  const fixture = {
    revision: 7,
    calls: [],
    settings: { ...defaultSettings(), dimensions },
    tables: new Map([["documents", {
      schema: documentSchema,
      rows: Array.from({ length: count }, (_, index) => [index + 1, `Document ${index + 1}`, `Content ${index + 1}`, [1, 0, 0]]),
    }]]),
  };
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname.startsWith("/v1/")) fixture.calls.push({
      path: url.pathname, query: url.searchParams, method: request.method(),
      body: request.postData() ? request.postDataJSON() : null,
    });
  });
  await page.route("**/healthz", (route) => reply(route, { status: "ok", version: "0.7.0", storage: "memory" }));
  await page.route("**/v1/**", async (route) => {
    const url = new URL(route.request().url());
    const path = url.pathname;
    const method = route.request().method();
    const body = route.request().postData() ? route.request().postDataJSON() : null;
    if (path === "/v1/settings/embeddings") {
      if (method === "PUT") {
        for (const key of ["provider", "model", "dimensions", "batch_size", "timeout_seconds", "max_concurrent_requests"]) {
          if (body[key] !== undefined) fixture.settings[key] = body[key];
        }
      }
      return reply(route, fixture.settings);
    }
    if (path === "/v1/settings/server") return reply(route, { version: "0.7.0", storage: "memory", authentication_required: false, compute: { backend: "cpu" }, limits: { max_response_rows: 10000 } });
    if (path === "/v1/tables") return reply(route, { revision: fixture.revision, tables: [...fixture.tables].map(([name, table]) => ({ name, row_count: table.rows.length, column_count: table.schema.length, index_count: 0 })) });
    const metadata = path.match(/^\/v1\/tables\/([^/]+)\/(schema|indexes)$/);
    if (metadata) {
      const table = fixture.tables.get(decodeURIComponent(metadata[1]));
      return reply(route, metadata[2] === "schema" ? { columns: table.schema } : { indexes: [] });
    }
    if (path === "/v1/embeddings") return reply(route, {
      provider: fixture.settings.provider, model: fixture.settings.model, dimensions: fixture.settings.dimensions,
      embeddings: body.input.map((_, index) => index ? [0, 1, 0] : [1, 0, 0]), usage: { total_tokens: 8 },
    });
    if (path === "/v1/vector/search") return reply(route, { columns: ["id", "title", "distance"], rows: [[1, "Document 1", 0.01]], row_count: 1, rows_examined: count });
    if (path === "/v1/admin/tables" && method === "POST") {
      fixture.tables.set(body.name, { schema: body.columns, rows: [] });
      fixture.revision += 1;
      return reply(route, command("CREATE TABLE", 0));
    }
    const admin = path.match(/^\/v1\/admin\/tables\/([^/]+)(\/rows)?$/);
    if (admin) {
      const name = decodeURIComponent(admin[1]);
      const table = fixture.tables.get(name);
      if (!admin[2] && method === "DELETE") {
        fixture.tables.delete(name);
        fixture.revision += 1;
        return reply(route, command("DROP TABLE", 0));
      }
      if (method === "GET") {
        const limit = Number(url.searchParams.get("limit") || 50);
        const offset = Number(url.searchParams.get("offset") || 0);
        return reply(route, { table: name, columns: table.schema.map((column) => column.name), schema: table.schema, rows: table.rows.slice(offset, offset + limit), total_rows: table.rows.length, limit, offset, revision: fixture.revision });
      }
      if (method === "PATCH") {
        const keyIndex = table.schema.findIndex((column) => column.name === body.key.column);
        const row = table.rows.find((row) => row[keyIndex] === body.key.value);
        for (const [name, value] of Object.entries(body.values)) row[table.schema.findIndex((column) => column.name === name)] = value;
        fixture.revision += 1;
        return reply(route, command("UPDATE"));
      }
    }
    const insert = path.match(/^\/v1\/tables\/([^/]+)\/rows$/);
    if (insert && method === "POST") {
      const table = fixture.tables.get(decodeURIComponent(insert[1]));
      table.rows.push(...body.rows.map((row) => table.schema.map((column) => row[column.name] ?? null)));
      fixture.revision += 1;
      return reply(route, command("INSERT", body.rows.length));
    }
    return reply(route, { error: { code: "unexpected_request", message: "Unexpected test API request" } }, 404);
  });
  await page.goto("/");
  await expect(page.locator("#status-label")).toHaveText("Connected");
  return fixture;
}

async function selectSearchTable(page) {
  await page.locator('.nav-item[data-view="search"]').click();
  await page.locator("#search-table").selectOption("documents");
  await expect(page.locator("#search-vector-column")).toHaveValue("embedding");
}

async function selectDataTable(page) {
  await page.locator('.nav-item[data-view="data"]').click();
  await page.locator("#admin-table").selectOption("documents");
  await expect(page.locator("#admin-rows tbody tr").first()).toContainText("Document 1");
}

async function documentsDraft(page) {
  const documents = [{ id: 10, title: "First", content: "First document text" }, { id: 11, title: "Second", content: "Second document text" }];
  await page.locator("#admin-text-column").selectOption("content");
  await page.locator("#admin-vector-column").selectOption("embedding");
  await page.locator("#admin-documents-json").fill(JSON.stringify(documents));
  return documents;
}

test("text search generates a query embedding with explicit settings before searching", async ({ page }) => {
  const fixture = await workspace(page);
  await selectSearchTable(page);
  await page.locator("#search-text").fill("How do I reset a password?");
  await page.locator("#search-submit").click();
  await expect(page.locator("#search-results")).toContainText("Document 1");
  const generation = fixture.calls.filter((call) => call.path === "/v1/embeddings");
  const searches = fixture.calls.filter((call) => call.path === "/v1/vector/search");
  expect(generation).toHaveLength(1);
  expect(generation[0].body).toEqual({ input: ["How do I reset a password?"], input_type: "query", expected_settings: { provider: "openai", model: "text-embedding-3-small", dimensions: 3 } });
  expect(searches).toHaveLength(1);
  expect(searches[0].body).toMatchObject({ table: "documents", vector_column: "embedding", query: [1, 0, 0] });
  expect(fixture.calls.indexOf(generation[0])).toBeLessThan(fixture.calls.indexOf(searches[0]));
});

test("dimension mismatch is explained before any text reaches the provider", async ({ page }) => {
  const fixture = await workspace(page, { dimensions: 4 });
  await selectSearchTable(page);
  await page.locator("#search-text").fill("Keep this text local");
  await page.locator("#search-submit").click();
  await expect(page.locator("#search-results")).toContainText(/dimension|match/i);
  expect(fixture.calls.filter((call) => ["/v1/embeddings", "/v1/vector/search"].includes(call.path))).toEqual([]);
});

test("creating a table sends a typed schema and opens the new table", async ({ page }) => {
  const fixture = await workspace(page);
  await page.locator('.nav-item[data-view="data"]').click();
  await page.locator("#admin-new-table").click();
  await page.locator("#admin-create-name").fill("notes");
  await page.locator("#admin-create-dimensions").fill("3");
  await page.locator("#admin-create-submit").click();
  await expect(page.locator("#admin-create-dialog")).not.toBeVisible();
  const requests = fixture.calls.filter((call) => call.path === "/v1/admin/tables" && call.method === "POST");
  expect(requests).toHaveLength(1);
  expect(requests[0].body.name).toBe("notes");
  expect(requests[0].body.columns).toEqual(expect.arrayContaining([
    expect.objectContaining({ name: "id", data_type: "INTEGER", nullable: false, unique: true }),
    expect.objectContaining({ name: "title", data_type: "TEXT" }),
    expect.objectContaining({ name: "content", data_type: "TEXT" }),
    expect.objectContaining({ name: "embedding", data_type: "VECTOR(3)" }),
  ]));
  await expect(page.locator("#admin-table")).toHaveValue("notes");
  expect(fixture.calls.filter((call) => call.path === "/v1/sql")).toEqual([]);
});

test("data browsing requests one server page at a time without downloading a SQL result", async ({ page }) => {
  const fixture = await workspace(page, { count: 123 });
  await selectDataTable(page);
  await expect(page.locator("#admin-rows tbody tr")).toHaveCount(50);
  await page.locator("#admin-next").click();
  await expect(page.locator("#admin-rows tbody tr").first()).toContainText("Document 51");
  await page.locator("#admin-next").click();
  await expect(page.locator("#admin-rows tbody tr")).toHaveCount(23);
  await expect(page.locator("#admin-next")).toBeDisabled();
  const pages = fixture.calls.filter((call) => call.path === "/v1/admin/tables/documents/rows" && call.method === "GET");
  expect(pages.map((call) => Number(call.query.get("offset") || 0))).toContain(50);
  expect(pages.map((call) => Number(call.query.get("offset") || 0))).toContain(100);
  expect(pages.every((call) => Number(call.query.get("limit")) === 50)).toBe(true);
  expect(fixture.calls.filter((call) => call.path === "/v1/sql")).toEqual([]);
});

test("editing a selected row sends its unique key and inspected revision", async ({ page }) => {
  const fixture = await workspace(page);
  await selectDataTable(page);
  await page.getByRole("button", { name: "Edit row 1", exact: true }).click();
  const draft = JSON.parse(await page.locator("#admin-row-json").inputValue());
  draft.title = "Revised title";
  await page.locator("#admin-row-json").fill(JSON.stringify(draft));
  await page.locator("#admin-save-row").click();
  await expect(page.locator("#admin-rows")).toContainText("Revised title");
  const updates = fixture.calls.filter((call) => call.method === "PATCH");
  expect(updates).toHaveLength(1);
  expect(updates[0].body).toMatchObject({ expected_revision: 7, key: { column: "id", value: 1 }, values: { title: "Revised title" } });
});

test("a stale row conflict preserves the edit draft and never retries automatically", async ({ page }) => {
  const fixture = await workspace(page);
  await selectDataTable(page);
  await page.route("**/v1/admin/tables/documents/rows", async (route) => {
    if (route.request().method() !== "PATCH") return route.fallback();
    return reply(route, { error: { code: "stale_revision", message: "The table changed; refresh before editing again" } }, 409);
  });
  await page.getByRole("button", { name: "Edit row 1", exact: true }).click();
  const draft = JSON.stringify({ id: 1, title: "Unsaved work", content: "Content 1", embedding: [1, 0, 0] }, null, 2);
  await page.locator("#admin-row-json").fill(draft);
  await page.locator("#admin-save-row").click();
  await expect(page.locator("#admin-edit-status")).toContainText(/changed|refresh/i);
  await expect(page.locator("#admin-row-json")).toHaveValue(draft);
  expect(fixture.calls.filter((call) => call.method === "PATCH")).toHaveLength(1);
});

test("a pending row update protects its draft from table, page, and row changes", async ({ page }) => {
  const fixture = await workspace(page, { count: 123 });
  await selectDataTable(page);
  const received = deferred();
  const release = deferred();
  await page.route("**/v1/admin/tables/documents/rows", async (route) => {
    if (route.request().method() !== "PATCH") return route.fallback();
    received.resolve();
    await release.promise;
    return route.fallback();
  });
  await page.getByRole("button", { name: "Edit row 1", exact: true }).click();
  const draft = JSON.stringify({ id: 1, title: "Keep this draft", content: "Content 1", embedding: [1, 0, 0] }, null, 2);
  await page.locator("#admin-row-json").fill(draft);
  await page.locator("#admin-save-row").click();
  await received.promise;
  for (const id of ["admin-table", "admin-page-size", "admin-clear-selection", "admin-new-row", "admin-next", "admin-refresh"]) {
    await expect(page.locator(`#${id}`)).toBeDisabled();
  }
  await expect(page.locator('#table-list [data-table="documents"]')).toBeDisabled();
  await expect(page.getByRole("button", { name: "Edit row 2", exact: true })).toBeDisabled();
  await expect(page.locator("#admin-row-json")).not.toBeEditable();
  await page.locator("#admin-row-json").press("ControlOrMeta+A");
  await page.locator("#admin-row-json").press("Backspace");
  await expect(page.locator("#admin-row-json")).toHaveValue(draft);
  release.resolve();
  await expect(page.locator("#admin-rows")).toContainText("Keep this draft");
  await expect(page.locator("#admin-table")).toBeEnabled();
  await expect(page.locator("#admin-row-json")).toBeEditable();
  const updates = fixture.calls.filter((call) => call.method === "PATCH");
  expect(updates).toHaveLength(1);
  expect(updates[0].body).toMatchObject({ expected_revision: 7, key: { column: "id", value: 1 }, values: { title: "Keep this draft" } });
});

test("an unsafe row key conflict reports its actual cause and preserves the draft", async ({ page }) => {
  const fixture = await workspace(page);
  await selectDataTable(page);
  await page.route("**/v1/admin/tables/documents/rows", async (route) => {
    if (route.request().method() !== "PATCH") return route.fallback();
    return reply(route, { error: { code: "unsafe_row_key", message: "row changes require a non-null value in a scalar UNIQUE column" } }, 409);
  });
  await page.getByRole("button", { name: "Edit row 1", exact: true }).click();
  const draft = JSON.stringify({ id: 1, title: "Unsaved change", content: "Content 1", embedding: [1, 0, 0] });
  await page.locator("#admin-row-json").fill(draft);
  await page.locator("#admin-save-row").click();
  await expect(page.locator("#admin-edit-status")).toHaveText("row changes require a non-null value in a scalar UNIQUE column");
  await expect(page.locator("#admin-edit-status")).not.toContainText("changed since you loaded");
  await expect(page.locator("#admin-row-json")).toHaveValue(draft);
  expect(fixture.calls.filter((call) => call.method === "PATCH")).toHaveLength(1);
});

test("deleting a table requires its exact name and current revision", async ({ page }) => {
  const fixture = await workspace(page);
  await selectDataTable(page);
  await page.locator("#admin-drop-table").click();
  await expect(page.locator("#admin-drop-submit")).toBeDisabled();
  await page.locator("#admin-drop-confirm").fill("DOCUMENTS");
  await expect(page.locator("#admin-drop-submit")).toBeDisabled();
  expect(fixture.calls.filter((call) => call.method === "DELETE")).toEqual([]);
  await page.locator("#admin-drop-confirm").fill("documents");
  await page.locator("#admin-drop-submit").click();
  await expect(page.locator("#admin-drop-dialog")).not.toBeVisible();
  const deletions = fixture.calls.filter((call) => call.method === "DELETE");
  expect(deletions).toHaveLength(1);
  expect(deletions[0]).toMatchObject({ path: "/v1/admin/tables/documents", body: { confirm_table: "documents", expected_revision: 7 } });
});

test("saving a provider key sends it once and clears it without storing it in the browser", async ({ page }) => {
  const fixture = await workspace(page);
  await page.locator('.nav-item[data-view="settings"]').click();
  await expect(page.locator("#embedding-model")).toHaveValue("text-embedding-3-small");
  const secret = "sk-browser-regression-only";
  await page.locator("#embedding-api-key").fill(secret);
  await page.locator("#embedding-save").click();
  await expect(page.locator("#embedding-api-key")).toHaveValue("");
  const saves = fixture.calls.filter((call) => call.path === "/v1/settings/embeddings" && call.method === "PUT");
  expect(saves).toHaveLength(1);
  expect(saves[0].body.api_key).toBe(secret);
  const stored = await page.evaluate(() => JSON.stringify({ local: { ...localStorage }, session: { ...sessionStorage } }));
  expect(stored).not.toContain(secret);
  await expect(page.locator("body")).not.toContainText(secret);
  await page.locator("#settings-refresh").click();
  await expect(page.locator("#embedding-api-key")).toHaveValue("");
});

test("document generation sends only source text and inserts completed vectors once", async ({ page }) => {
  const fixture = await workspace(page);
  await selectDataTable(page);
  const documents = await documentsDraft(page);
  const received = deferred();
  const release = deferred();
  await page.route("**/v1/embeddings", async (route) => {
    received.resolve();
    await release.promise;
    return reply(route, { provider: "openai", model: "text-embedding-3-small", dimensions: 3, embeddings: [[1, 0, 0], [0, 1, 0]], usage: { total_tokens: 8 } });
  });
  await page.locator("#admin-embed-insert").dblclick();
  await received.promise;
  await expect(page.locator("#admin-embed-insert")).toBeDisabled();
  release.resolve();
  await expect(page.locator("#admin-ingest-status")).toContainText(/inserted|added/i);
  const generations = fixture.calls.filter((call) => call.path === "/v1/embeddings");
  const inserts = fixture.calls.filter((call) => call.path === "/v1/tables/documents/rows" && call.method === "POST");
  expect(generations).toHaveLength(1);
  expect(generations[0].body).toEqual({ input: documents.map((document) => document.content), input_type: "document", expected_settings: { provider: "openai", model: "text-embedding-3-small", dimensions: 3 } });
  expect(inserts).toHaveLength(1);
  expect(inserts[0].body.rows).toEqual(documents.map((document, index) => ({ ...document, embedding: index ? [0, 1, 0] : [1, 0, 0] })));
});

test("a provider error keeps document drafts and prevents insertion", async ({ page }) => {
  const fixture = await workspace(page);
  await selectDataTable(page);
  const documents = await documentsDraft(page);
  await page.route("**/v1/embeddings", (route) => reply(route, { error: { code: "embedding_unavailable", message: "Provider unavailable; the request was not retried" } }, 502));
  await page.locator("#admin-embed-insert").click();
  await expect(page.locator("#admin-ingest-status")).toContainText("Provider unavailable");
  await expect(page.locator("#admin-documents-json")).toHaveValue(JSON.stringify(documents));
  expect(fixture.calls.filter((call) => call.path === "/v1/tables/documents/rows" && call.method === "POST")).toEqual([]);
});

test("changing the connection during generation prevents a late response from inserting documents", async ({ page }) => {
  const fixture = await workspace(page);
  await selectDataTable(page);
  await documentsDraft(page);
  const received = deferred();
  const release = deferred();
  const completed = deferred();
  await page.route("**/v1/embeddings", async (route) => {
    received.resolve();
    await release.promise;
    await reply(route, { provider: "openai", model: "text-embedding-3-small", dimensions: 3, embeddings: [[1, 0, 0], [0, 1, 0]], usage: { total_tokens: 8 } });
    completed.resolve();
  });
  await page.locator("#admin-embed-insert").click();
  await received.promise;
  await page.locator("#open-token").click();
  await page.locator("#token-input").fill("replacement-session");
  await page.locator("#token-input").press("Enter");
  await expect(page.locator("#token-dialog")).not.toBeVisible();
  await expect(page.locator("#status-label")).toHaveText("Connected");
  release.resolve();
  await completed.promise;
  await page.evaluate(() => new Promise(requestAnimationFrame));
  expect(fixture.calls.filter((call) => call.path === "/v1/tables/documents/rows" && call.method === "POST")).toEqual([]);
});
