"use strict";

const { test, expect } = require("@playwright/test");

const column = (name, data_type) => ({ name, data_type, nullable: false, unique: false });
const schemas = {
  documents: [
    column("id", "INTEGER"),
    column("title", "TEXT"),
    column("score", "DOUBLE"),
    column("published", "BOOLEAN"),
    column("embedding", "VECTOR(3)"),
  ],
  table_a: [column("name_a", "TEXT"), column("embedding_a", "VECTOR(2)")],
  table_b: [column("name_b", "TEXT"), column("embedding_b", "VECTOR(3)")],
};

function deferred() {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
}

async function json(route, body, status = 200) {
  await route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
}

function embeddingSettings(overrides = {}) {
  return {provider:"openai",model:"text-embedding-3-small",dimensions:3,batch_size:32,timeout_seconds:60,max_concurrent_requests:4,configured:true,persistence:"memory",providers:[{id:"openai",label:"OpenAI",configured:true,models:[{id:"text-embedding-3-small",dimensions:[1536],default_dimensions:1536},{id:"text-embedding-3-large",dimensions:[3072],default_dimensions:3072}]},{id:"voyage",label:"Voyage AI",configured:false,models:[{id:"voyage-4",dimensions:[256,512,1024,2048],default_dimensions:1024}]}],...overrides};
}

async function mockApi(page, names = ["documents"]) {
  await page.route("**/healthz", (route) => json(route, {
    status: "ok", version: "0.7.0", storage: "memory",
  }));
  await page.route("**/v1/**", async (route) => {
    const pathname = new URL(route.request().url()).pathname;
    if (pathname === "/v1/settings/embeddings") {
      await json(route, embeddingSettings());
      return;
    }
    if (pathname === "/v1/settings/server") {
      await json(route, {version:"0.7.0",storage:"memory",authentication:false,compute:{device:"auto",gpu_enabled:false,gpu_min_elements:8388608,gpu_cache_bytes:536870912},limits:{max_json_payload_bytes:33554432,max_bulk_rows:10000,max_response_rows:10000,max_search_limit:1000},capacity:{workers:2,max_concurrent_database_tasks:4}});
      return;
    }
    if (pathname === "/v1/tables") {
      await json(route, {
        revision: 1,
        tables: names.map((name) => ({ name, row_count: 3, index_count: 0 })),
      });
      return;
    }
    const match = pathname.match(/^\/v1\/tables\/([^/]+)\/(schema|indexes)$/);
    if (match && schemas[match[1]]) {
      await json(route, match[2] === "schema"
        ? { columns: schemas[match[1]] }
        : { indexes: [] });
      return;
    }
    await json(route, { error: { code: "not_found", message: "Unexpected test API request" } }, 404);
  });
}

async function openConsole(page, names) {
  await mockApi(page, names);
  await page.goto("/");
  await expect(page.locator("#status-label")).toHaveText("Connected");
  await page.locator('.nav-item[data-view="console"]').click();
}

async function openSearch(page) {
  await openConsole(page);
  await page.locator('.nav-item[data-view="search"]').click();
  await page.locator("#search-table").selectOption("documents");
  await expect(page.locator("#search-vector-column")).toHaveValue("embedding");
  await page.locator("#search-mode-vector").click();
  await page.locator(".search-options summary").click();
}

test("the console keeps controls and statistics inside the layout at desktop and mobile widths", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await openConsole(page);
  await page.locator("#sql-editor").fill(`SELECT '${"x".repeat(1000)}' AS long_value;`);
  for (const width of [1440, 1280, 1024, 820, 768, 390]) {
    await test.step(`${width}px viewport`, async () => {
      await page.setViewportSize({ width, height: 1000 });
      const layout = await page.evaluate(() => {
        const panel = document.querySelector(".editor-panel").getBoundingClientRect();
        return {
          viewport: document.documentElement.clientWidth,
          page: document.documentElement.scrollWidth,
          panel: { left: panel.left, right: panel.right, top: panel.top, bottom: panel.bottom },
          controls: Array.from(document.querySelectorAll(".editor-actions button, .editor-actions select"), (element) => {
            const rect = element.getBoundingClientRect();
            return {
              id: element.id, left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom,
              scrollWidth: element.scrollWidth, clientWidth: element.clientWidth,
            };
          }),
          statistics: Array.from(document.querySelectorAll(".stat-card"), (element) => {
            const rect = element.getBoundingClientRect();
            return { left: rect.left, right: rect.right };
          }),
        };
      });
      expect(layout.page, "the document must not scroll horizontally").toBeLessThanOrEqual(layout.viewport + 1);
      for (const control of layout.controls) {
        expect(control.left, `${control.id} left edge`).toBeGreaterThanOrEqual(layout.panel.left - 1);
        expect(control.right, `${control.id} right edge`).toBeLessThanOrEqual(layout.panel.right + 1);
        expect(control.top, `${control.id} top edge`).toBeGreaterThanOrEqual(layout.panel.top - 1);
        expect(control.bottom, `${control.id} bottom edge`).toBeLessThanOrEqual(layout.panel.bottom + 1);
        expect(control.scrollWidth, `${control.id} must not clip its content`).toBeLessThanOrEqual(control.clientWidth + 1);
      }
      for (const statistic of layout.statistics) {
        expect(statistic.left).toBeGreaterThanOrEqual(-1);
        expect(statistic.right).toBeLessThanOrEqual(layout.viewport + 1);
      }
    });
  }
});

test("repeated SQL shortcuts send one request while the query is running", async ({ page }) => {
  await openConsole(page);
  const received = deferred();
  const release = deferred();
  let requests = 0;
  await page.route("**/v1/sql", async (route) => {
    requests += 1;
    received.resolve();
    await release.promise;
    await json(route, { results: [{ type: "command", tag: "INSERT", rows_affected: 1 }] });
  });
  const editor = page.getByRole("textbox", { name: "SQL editor", exact: true });
  await editor.fill("INSERT INTO documents VALUES (4, 'Only once');");
  await editor.press("Control+Enter");
  await received.promise;
  await editor.press("Control+Enter");
  await editor.press("Meta+Enter");
  await expect(page.locator("#run-sql")).toBeDisabled();
  await expect(page.locator("#analyze-sql")).toBeDisabled();
  release.resolve();
  await expect(page.locator("#editor-status")).toHaveText("Complete");
  await expect(page.locator("#run-sql")).toBeEnabled();
  expect(requests).toBe(1);
});

test("a SQL validation error leaves the reachable server connected", async ({ page }) => {
  await openConsole(page);
  await page.route("**/v1/sql", (route) => json(route, {
    error: { code: "invalid_sql", message: "Unknown column missing_column" },
  }, 400));
  await page.locator("#sql-editor").fill("SELECT missing_column FROM documents;");
  await page.locator("#run-sql").click();
  await expect(page.locator("#results-title")).toHaveText("Query failed");
  await expect(page.locator("#sql-results")).toContainText("Unknown column missing_column");
  await expect(page.locator("#status-label")).toHaveText("Connected");
  await expect(page.locator("#run-sql")).toBeEnabled();
});

test("a lost connection keeps the last tables and query available for reconnecting", async ({ page }) => {
  await openConsole(page);
  const query = "SELECT title FROM documents;";
  await page.locator("#sql-editor").fill(query);
  let offline = true;
  await page.route("**/v1/tables", async (route) => {
    if (offline) {
      await route.abort("connectionfailed");
      return;
    }
    await json(route, {
      revision: 1, tables: [{ name: "documents", row_count: 3, index_count: 0 }],
    });
  });
  await page.locator("#refresh-tables").click();
  await expect(page.locator("#status-label")).toHaveText("Disconnected");
  await expect(page.locator("#connection-notice")).toBeVisible();
  await expect(page.locator('#table-list button[data-table="documents"]')).toBeVisible();
  await expect(page.locator("#stat-rows")).toHaveText("3");
  await expect(page.locator("#sql-editor")).toHaveValue(query);
  offline = false;
  await page.getByRole("button", { name: "Reconnect", exact: true }).click();
  await expect(page.locator("#status-label")).toHaveText("Connected");
  await expect(page.locator("#connection-notice")).not.toBeVisible();
  await expect(page.locator("#sql-editor")).toHaveValue(query);
});

test("a lost SQL response explains the uncertain outcome and reconnecting never repeats the write", async ({ page }) => {
  await openConsole(page);
  let requests = 0;
  await page.route("**/v1/sql", async (route) => {
    requests += 1;
    await route.abort("connectionfailed");
  });
  const query = "INSERT INTO documents VALUES (4, 'Connection lost during write');";
  await page.locator("#sql-editor").fill(query);
  await page.locator("#run-sql").click();
  await expect(page.locator("#results-title")).toHaveText("Query failed");
  await expect(page.locator("#sql-results")).toContainText("connection was lost before a response arrived");
  await expect(page.locator("#sql-results")).toContainText("may still finish on the server");
  await expect(page.locator("#sql-results")).toContainText("check its result before running it again");
  await expect(page.locator("#status-label")).toHaveText("Disconnected");
  await expect(page.locator("#run-sql")).toBeEnabled();
  await page.getByRole("button", { name: "Reconnect", exact: true }).click();
  await expect(page.locator("#status-label")).toHaveText("Connected");
  await expect(page.locator("#results-title")).toHaveText("Query failed");
  await expect(page.locator("#sql-results")).toContainText("may still finish on the server");
  await expect(page.locator("#sql-editor")).toHaveValue(query);
  expect(requests).toBe(1);
});

test("a capacity rejection reports a busy server instead of a lost connection", async ({ page }) => {
  await openConsole(page);
  await page.route("**/v1/sql", (route) => json(route, {
    error: { code: "database_busy", message: "Capacity temporarily exhausted" },
  }, 503));
  await page.locator("#sql-editor").fill("SELECT title FROM documents;");
  await page.locator("#run-sql").click();
  await expect(page.locator("#status-label")).toHaveText("Server busy");
  await expect(page.locator("#connection-notice")).toContainText("The server is busy");
  await expect(page.locator("#sql-results")).toContainText("Capacity temporarily exhausted");
  await expect(page.locator("#run-sql")).toBeEnabled();
});

test("a SELECT avoids catalog reloads and leaves the help view open", async ({ page }) => {
  await openConsole(page);
  await page.locator('#table-list button[data-table="documents"]').click();
  await expect(page.locator("#table-inspector h3")).toHaveText("documents");
  const metadataRequests = [];
  page.on("request", (request) => {
    const pathname = new URL(request.url()).pathname;
    if (pathname.startsWith("/v1/tables")) metadataRequests.push(pathname);
  });
  const received = deferred();
  const release = deferred();
  await page.route("**/v1/sql", async (route) => {
    received.resolve();
    await release.promise;
    await json(route, { results: [{
      type: "query", columns: ["id"], rows: [[1]], row_count: 1, rows_examined: 1,
    }] });
  });
  await page.locator("#sql-editor").fill("SELECT id FROM documents;");
  await page.locator("#run-sql").click();
  await received.promise;
  await page.getByRole("button", { name: "Help", exact: true }).click();
  await expect(page.locator("#view-guide")).toBeVisible();
  release.resolve();
  await expect(page.locator("#editor-status")).toHaveText("Complete");
  await expect(page.locator("#run-sql")).toBeEnabled();
  await expect(page.locator("#view-guide")).toBeVisible();
  await expect(page.locator("#view-title")).toHaveText("Help");
  expect(metadataRequests).toEqual([]);
});

test("an explicit refresh replaces cached schemas when a table is recreated", async ({ page }) => {
  await openConsole(page);
  let recreated = false;
  let schemaRequests = 0;
  await page.route("**/v1/tables/documents/schema", async (route) => {
    schemaRequests += 1;
    await json(route, { columns: recreated
      ? [column("new_title", "TEXT"), column("new_embedding", "VECTOR(5)")]
      : schemas.documents });
  });
  await page.locator('#table-list button[data-table="documents"]').click();
  await expect(page.locator("#table-inspector")).toContainText("embedding");
  await page.locator('.nav-item[data-view="search"]').click();
  await page.locator("#search-table").selectOption("documents");
  await expect(page.locator("#search-vector-column")).toHaveValue("embedding");
  recreated = true;
  // The list keeps the same revision and row counts to require explicit invalidation.
  await page.locator("#refresh-tables").click();
  await expect(page.locator("#search-vector-column")).toHaveValue("new_embedding");
  await expect(page.locator("#search-select")).toHaveValue("new_title");
  await expect(page.locator("#dimension-hint")).toContainText("5");
  await expect(page.locator("#table-inspector")).toContainText("new_embedding");
  await expect(page.locator("#table-inspector")).not.toContainText("published");
  await expect(page.locator("#view-search")).toBeVisible();
  expect(schemaRequests).toBe(2);
});

test("a late unauthorized response cannot reopen the token dialog after reconnecting", async ({ page }) => {
  await openConsole(page);
  const received = deferred();
  const release = deferred();
  const completed = deferred();
  await page.route("**/v1/sql", async (route) => {
    received.resolve();
    await release.promise;
    await json(route, { error: { code: "unauthorized", message: "Old token expired" } }, 401);
    completed.resolve();
  });
  await page.locator("#sql-editor").fill("SELECT id FROM documents;");
  await page.locator("#run-sql").click();
  await received.promise;
  await page.locator("#open-token").click();
  await page.locator("#token-input").fill("replacement-token");
  const reconnected = page.waitForRequest((request) =>
    new URL(request.url()).pathname === "/v1/tables"
    && request.headers().authorization === "Bearer replacement-token");
  await page.locator("#token-input").press("Enter");
  await reconnected;
  await expect(page.locator("#status-label")).toHaveText("Connected");
  release.resolve();
  await completed.promise;
  await page.evaluate(() => new Promise(requestAnimationFrame));
  await expect(page.locator("#token-dialog")).not.toBeVisible();
  await expect(page.locator("#status-label")).toHaveText("Connected");
  await expect(page.locator("#toast-region")).not.toContainText("Authentication required");
  await expect(page.locator("#run-sql")).toBeEnabled();
  await expect(page.locator("#results-title")).toHaveText("Request interrupted");
  await expect(page.locator("#sql-results")).toContainText("may still finish on the server");
  await expect(page.locator("#editor-status")).toHaveText("Previous request may still finish");
});

test("overlapping schema refreshes preserve search choices and edits made while loading", async ({ page }) => {
  await openConsole(page);
  const columns = [...schemas.documents, column("second_embedding", "VECTOR(3)")];
  const received = [deferred(), deferred()];
  const release = [deferred(), deferred()];
  const completed = [deferred(), deferred()];
  let schemaRequests = 0;
  await page.route("**/v1/tables/documents/schema", async (route) => {
    const index = schemaRequests++ - 1;
    if (index >= 0) {
      received[index].resolve();
      await release[index].promise;
    }
    await json(route, { columns });
    if (index >= 0) completed[index].resolve();
  });
  await page.locator('.nav-item[data-view="search"]').click();
  await page.locator("#search-table").selectOption("documents");
  await page.locator("#search-vector-column").selectOption("second_embedding");
  await page.locator(".search-options summary").click();
  await page.locator("#filter-column").selectOption("published");
  await page.locator("#search-select").fill("score");
  await page.locator("#refresh-tables").click();
  await received[0].promise;
  await expect(page.locator("#search-vector-column")).toBeDisabled();
  await page.locator("#search-select").fill("score, title");
  await page.locator("#refresh-tables").click();
  await received[1].promise;
  await page.locator("#search-select").fill("title, id");
  release[1].resolve();
  await expect(page.locator("#search-vector-column")).toBeEnabled();
  await expect(page.locator("#search-vector-column")).toHaveValue("second_embedding");
  await expect(page.locator("#filter-column")).toHaveValue("published");
  await expect(page.locator("#search-select")).toHaveValue("title, id");
  release[0].resolve();
  await completed[0].promise;
  await page.evaluate(() => new Promise(requestAnimationFrame));
  await expect(page.locator("#search-vector-column")).toHaveValue("second_embedding");
  await expect(page.locator("#filter-column")).toHaveValue("published");
  await expect(page.locator("#search-select")).toHaveValue("title, id");
});

test("the next catalog poll retries failed inspector and search schemas without a revision change", async ({ page }) => {
  await openConsole(page);
  let schemaRequests = 0;
  await page.route("**/v1/tables/documents/schema", async (route) => {
    schemaRequests += 1;
    if (schemaRequests <= 2) {
      await json(route, { error: { code: "database_busy", message: "Schema temporarily unavailable" } }, 503);
      return;
    }
    await json(route, { columns: schemas.documents });
  });
  await page.locator('#table-list button[data-table="documents"]').click();
  await expect(page.locator("#table-inspector")).toContainText("Schema temporarily unavailable");
  await page.locator('.nav-item[data-view="search"]').click();
  await page.locator("#search-table").selectOption("documents");
  await expect(page.locator("#toast-region .error")).toHaveCount(2);
  await expect(page.locator("#search-vector-column")).toBeEnabled();
  await expect(page.locator("#search-vector-column")).toHaveValue("");
  await page.evaluate(() => loadTables({ quiet: true }));
  await expect(page.locator("#search-vector-column")).toHaveValue("embedding");
  await expect(page.locator("#table-inspector h3")).toHaveText("documents");
  await expect(page.locator("#table-inspector")).toContainText("embedding");
  await expect(page.locator("#status-label")).toHaveText("Connected");
  await expect(page.locator("#stat-revision")).toHaveText("1");
  expect(schemaRequests).toBe(3);
});

test("recovering online reloads a cached schema even when the catalog revision is unchanged", async ({ page }) => {
  await openConsole(page);
  let recreated = false;
  await page.route("**/v1/tables/documents/schema", (route) => json(route, { columns: recreated
    ? [column("replacement_title", "TEXT"), column("replacement_vector", "VECTOR(4)")]
    : schemas.documents }));
  await page.locator('.nav-item[data-view="search"]').click();
  await page.locator("#search-table").selectOption("documents");
  await expect(page.locator("#search-vector-column")).toHaveValue("embedding");
  await page.evaluate(() => window.dispatchEvent(new Event("offline")));
  await expect(page.locator("#status-label")).toHaveText("Disconnected");
  recreated = true;
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await expect(page.locator("#search-vector-column")).toHaveValue("replacement_vector");
  await expect(page.locator("#search-select")).toHaveValue("replacement_title");
  await expect(page.locator("#dimension-hint")).toContainText("4");
  await expect(page.locator("#status-label")).toHaveText("Connected");
  await expect(page.locator("#stat-revision")).toHaveText("1");
});

test("a SQL timeout explains the uncertain outcome and never retries the POST automatically", async ({ page }) => {
  await page.clock.install();
  await openConsole(page);
  const received = deferred();
  const release = deferred();
  const completed = deferred();
  let requests = 0;
  await page.route("**/v1/sql", async (route) => {
    requests += 1;
    received.resolve();
    await release.promise;
    await json(route, { results: [{ type: "command", tag: "INSERT", rows_affected: 1 }] });
    completed.resolve();
  });
  const query = "INSERT INTO documents VALUES (4, 'A slow write');";
  await page.locator("#sql-editor").fill(query);
  await page.locator("#run-sql").click();
  await received.promise;
  await page.clock.fastForward(60_001);
  await expect(page.locator("#results-title")).toHaveText("Query failed");
  await expect(page.locator("#sql-results")).toContainText("request timed out");
  await expect(page.locator("#sql-results")).toContainText("may still finish on the server");
  await expect(page.locator("#status-label")).toHaveText("Request timed out");
  await expect(page.locator("#run-sql")).toBeEnabled();
  await expect(page.locator("#sql-editor")).toHaveValue(query);
  await page.clock.fastForward(15_001);
  release.resolve();
  await completed.promise;
  await expect(page.locator("#results-title")).toHaveText("Query failed");
  expect(requests).toBe(1);
});

test("a delayed table inspection cannot replace a newer selection", async ({ page }) => {
  await openConsole(page, ["table_a", "table_b"]);
  const received = deferred();
  const release = deferred();
  const completed = deferred();
  await page.route("**/v1/tables/table_a/schema", async (route) => {
    received.resolve();
    await release.promise;
    await json(route, { columns: schemas.table_a });
    completed.resolve();
  });
  await page.locator('#table-list button[data-table="table_a"]').click();
  await received.promise;
  await page.locator('#table-list button[data-table="table_b"]').click();
  await expect(page.locator("#table-inspector h3")).toHaveText("table_b");
  release.resolve();
  await completed.promise;
  await page.evaluate(() => new Promise(requestAnimationFrame));
  await expect(page.locator("#table-inspector h3")).toHaveText("table_b");
  await expect(page.locator("#table-inspector")).toContainText("embedding_b");
  await expect(page.locator("#table-inspector")).not.toContainText("embedding_a");
});

test("a delayed schema cannot replace the selected search table's columns", async ({ page }) => {
  await openConsole(page, ["table_a", "table_b"]);
  const received = deferred();
  const release = deferred();
  const completed = deferred();
  await page.route("**/v1/tables/table_a/schema", async (route) => {
    received.resolve();
    await release.promise;
    await json(route, { columns: schemas.table_a });
    completed.resolve();
  });
  await page.locator('.nav-item[data-view="search"]').click();
  await page.locator("#search-table").selectOption("table_a");
  await received.promise;
  await page.locator("#search-table").selectOption("table_b");
  await expect(page.locator("#search-vector-column")).toHaveValue("embedding_b");
  release.resolve();
  await completed.promise;
  await page.evaluate(() => new Promise(requestAnimationFrame));
  await expect(page.locator("#search-vector-column")).toHaveValue("embedding_b");
  await expect(page.locator("#search-vector-column option")).toHaveText([
    "Choose a vector", "embedding_b · VECTOR(3)",
  ]);
  await expect(page.locator("#search-select")).toHaveValue("name_b");
  await expect(page.locator("#dimension-hint")).toContainText("3");
});

test("blocked session storage still allows startup and an in-memory API token", async ({ page }) => {
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(() => {
    Object.defineProperty(window, "sessionStorage", {
      configurable: true,
      get() { throw new DOMException("Storage disabled", "SecurityError"); },
    });
  });
  await openConsole(page);
  const authorization = deferred();
  await page.route("**/v1/tables", async (route) => {
    authorization.resolve(route.request().headers().authorization);
    await json(route, { revision: 1, tables: [] });
  });
  await page.locator("#open-token").click();
  await page.locator("#token-input").fill("test-token");
  await page.locator("#token-input").press("Enter");
  expect(await authorization.promise).toBe("Bearer test-token");
  await expect(page.locator("#token-dialog")).not.toBeVisible();
  await expect(page.locator("#status-label")).toHaveText("Connected");
  expect(errors).toEqual([]);
});

for (const invalid of [
  { name: "empty vector component", vector: "1,,0", error: /vector|number|empty/i },
  { name: "wrong vector dimension", vector: "1,0", error: /dimension|3/i },
  { name: "non-numeric filter", filter: "score", value: "not a number", error: /number|numeric|double/i },
  { name: "empty numeric filter", filter: "score", value: " ", error: /number|numeric|double|empty|required/i },
  { name: "fractional integer filter", filter: "id", value: "1.5", error: /integer/i },
  { name: "invalid boolean filter", filter: "published", value: "maybe", error: /boolean|true|false/i },
]) {
  test(`${invalid.name} is explained without sending a search request`, async ({ page }) => {
    await openSearch(page);
    let searches = 0;
    await page.route("**/v1/vector/search", async (route) => {
      searches += 1;
      await json(route, { columns: [], rows: [], row_count: 0, rows_examined: 0 });
    });
    await page.locator("#search-vector").fill(invalid.vector || "1,0,0");
    if (invalid.filter) {
      await page.locator("#filter-column").selectOption(invalid.filter);
      await page.locator("#filter-value").fill(invalid.value);
    }
    await page.locator("#search-submit").click();
    await expect(page.locator("#search-results")).toContainText(invalid.error);
    await expect(page.locator("#toast-region .error").last()).toContainText(invalid.error);
    await expect(page.locator("#status-label")).toHaveText("Connected");
    expect(searches).toBe(0);
  });
}

test("large results keep the visible table bounded and expand vectors on demand", async ({ page }) => {
  await openConsole(page);
  const sentinel = 987654321;
  const vector = [...Array.from({ length: 63 }, (_, index) => index / 10), sentinel];
  const rows = Array.from({ length: 10_000 }, (_, index) => [index + 1, `row-${index + 1}`, vector]);
  await page.route("**/v1/sql", (route) => json(route, { results: [{
    type: "query", columns: ["id", "title", "embedding"], rows,
    row_count: rows.length, rows_examined: rows.length,
    schema: [column("id", "INTEGER"), column("title", "TEXT"), column("embedding", "VECTOR(64)")],
  }] }));
  await page.locator("#sql-editor").fill("SELECT * FROM documents;");
  await page.locator("#run-sql").click();
  const results = page.locator("#sql-results");
  const visibleRows = results.locator("tbody tr");
  await expect(visibleRows).toHaveCount(100);
  await expect(visibleRows.first().locator("td").first()).toHaveText("1");
  await expect(results).not.toContainText(String(sentinel));
  expect(await results.locator("[title]").evaluateAll((elements) => elements.map((element) => element.title)))
    .not.toEqual(expect.arrayContaining([expect.stringContaining(String(sentinel))]));
  const expand = results.getByRole("button", { name: "Show full value for embedding, row 1", exact: true });
  await expand.click();
  await expect(visibleRows.first()).toContainText(String(sentinel));
  await expect(results.getByRole("button", { name: "Collapse value for embedding, row 1", exact: true }))
    .toHaveAttribute("aria-expanded", "true");
  await results.getByRole("button", { name: "Collapse value for embedding, row 1", exact: true }).click();
  await expect(expand).toHaveAttribute("aria-expanded", "false");
  await expect(results).not.toContainText(String(sentinel));
  await results.getByRole("button", { name: "Next page", exact: true }).click();
  await expect(visibleRows).toHaveCount(100);
  await expect(visibleRows.first().locator("td").first()).toHaveText("101");
  await results.getByRole("button", { name: "Last page", exact: true }).click();
  await expect(visibleRows.first().locator("td").first()).toHaveText("9901");
  await expect(visibleRows.last().locator("td").first()).toHaveText("10000");
  await expect(results.getByRole("button", { name: "Next page", exact: true })).toBeDisabled();
  await results.getByRole("button", { name: "Previous page", exact: true }).click();
  await expect(visibleRows.first().locator("td").first()).toHaveText("9801");
  await results.getByRole("button", { name: "First page", exact: true }).click();
  await expect(visibleRows.first().locator("td").first()).toHaveText("1");
  await results.getByLabel("Rows per page", { exact: true }).selectOption("50");
  await expect(visibleRows).toHaveCount(50);
  await expect(results.getByRole("button", { name: "Previous page", exact: true })).toBeDisabled();
  await results.getByLabel("Rows per page", { exact: true }).selectOption("250");
  await expect(visibleRows).toHaveCount(250);
});
