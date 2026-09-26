"use strict";

const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => Array.from(root.querySelectorAll(selector));
const QUICK_START_STORAGE_KEY = "vectors.quickStart.dismissed.v1";

function storedToken() {
  try { return sessionStorage.getItem("vectors.apiToken") || ""; }
  catch { return ""; }
}

const state = {
  token: storedToken(),
  session: 0,
  requests: new Set(),
  tablesRequest: null,
  tablesGeneration: 0,
  schemaGeneration: 0,
  schemaRequests: new Map(),
  revision: null,
  inspectorGeneration: 0,
  searchColumnsGeneration: 0,
  sqlBusy: false,
  searchBusy: false,
  inspectorFailed: false,
  searchColumnsFailed: false,
  connection: "connecting",
  tables: [],
  schemas: new Map(),
  activeTable: null,
  view: "search",
  preferences: readBrowserPreferences(),
  refreshTimer: null,
  searchMode: "text",
  searchIntentGeneration: 0,
  embeddings: null,
  embeddingRequest: null,
  embeddingGeneration: 0,
  embeddingSaveBusy: false,
  serverSettings: null,
  graph: newGraphState(),
  reranking: null,
  rerankingGeneration: 0,
  rerankingBusy: false,
  relationships: { items: [], revision: null, generation: 0, schemaGeneration: 0, busy: false, loading: false, error: "", sourceSchema: [], targetSchema: [] },
  admin: { table: "", generation: 0, offset: 0, limit: 50, total: 0, revision: null, columns: [], schema: [], rows: [], selected: null, busy: false },
};

const examples = {
  quickstart: `CREATE TABLE IF NOT EXISTS documents (
  id INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  category TEXT,
  embedding VECTOR(3)
);

CREATE INDEX IF NOT EXISTS documents_category_idx
  ON documents USING HASH (category);

INSERT INTO documents VALUES
  (1, 'Rust for data systems', 'tech', ARRAY[1, 0, 0]),
  (2, 'A practical cooking guide', 'food', ARRAY[0, 1, 0]),
  (3, 'Inside database engines', 'tech', ARRAY[0.82, 0.18, 0])
ON CONFLICT (id) DO UPDATE SET
  title = excluded.title,
  category = excluded.category,
  embedding = excluded.embedding;`,
  hybrid: `SELECT
  id,
  title,
  category,
  cosine_distance(embedding, ARRAY[1, 0, 0]) AS distance
FROM documents
WHERE category = 'tech'
ORDER BY distance
LIMIT 5;`,
  upsert: `INSERT INTO documents VALUES
  (3, 'Database internals, revised', 'tech', ARRAY[0.9, 0.1, 0])
ON CONFLICT (id) DO UPDATE SET
  title = excluded.title,
  category = excluded.category,
  embedding = normalize(excluded.embedding);`,
  aggregate: `SELECT
  category,
  COUNT(*) AS documents,
  AVG(vector_norm(embedding)) AS average_norm
FROM documents
GROUP BY category
HAVING COUNT(*) > 0
ORDER BY documents DESC;`,
};

function node(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

function clear(element) {
  element.replaceChildren();
  return element;
}

function setSidebarOpen(open) {
  document.body.classList.toggle("sidebar-open", open);
  $("#mobile-menu").setAttribute("aria-expanded", String(open));
}

function revealQuickStart() {
  let dismissed = false;
  try {
    dismissed = localStorage.getItem(QUICK_START_STORAGE_KEY) === "true";
  } catch {
    // The guide remains available when browser storage is disabled.
  }
  $("#quick-start").hidden = dismissed;
}

function dismissQuickStart() {
  $("#quick-start").hidden = true;
  try {
    localStorage.setItem(QUICK_START_STORAGE_KEY, "true");
  } catch {
    // Dismissing still works for this page even without persistent storage.
  }
}

function openGuide(focusTarget = null) {
  switchView("guide");
  if (!focusTarget) return;
  window.requestAnimationFrame(() => {
    const target = $(focusTarget);
    target?.focus({ preventScroll: true });
    target?.scrollIntoView({ behavior: "smooth", block: "start" });
  });
}

function staleRequest() {
  return Object.assign(new Error("Request superseded"), { stale: true });
}

async function request(path, options = {}) {
  const method = options.method || "GET";
  const { timeout = method === "GET" ? 15000 : 60000, ...fetchOptions } = options;
  const session = state.session;
  const controller = new AbortController();
  const timer = window.setTimeout(() => controller.abort(), timeout);
  state.requests.add(controller);
  const headers = new Headers(options.headers || {});
  headers.set("accept", "application/json");
  if (options.body) headers.set("content-type", "application/json");
  if (state.token) headers.set("authorization", `Bearer ${state.token}`);
  try {
    const response = await fetch(path, { ...fetchOptions, headers, cache: "no-store", signal: controller.signal });
    if (session !== state.session) throw staleRequest();
    const type = response.headers.get("content-type") || "";
    let payload = null;
    if (type.includes("application/json")) {
      try { payload = await response.json(); }
      catch (error) {
        if (controller.signal.aborted) throw error;
        throw Object.assign(new Error("The server returned invalid JSON. Check the API connection."), { kind: "protocol" });
      }
    }
    if (session !== state.session) throw staleRequest();
    if (!response.ok) {
      const error = new Error(payload?.error?.message || `${response.status} ${response.statusText}`);
      error.status = response.status;
      error.code = payload?.error?.code;
      throw error;
    }
    if (payload === null) {
      throw Object.assign(new Error("The server returned an unexpected response. Check the API connection."), { kind: "protocol" });
    }
    return payload;
  } catch (error) {
    if (session !== state.session) throw staleRequest();
    if (controller.signal.aborted) {
      const message = method === "GET"
        ? "The server took too long to respond. Try reconnecting."
        : "The request timed out. It may still finish on the server; check its result before running it again.";
      throw Object.assign(new Error(message), { kind: "timeout" });
    }
    if (error instanceof TypeError) {
      const message = method === "GET"
        ? "Cannot reach the server. Check that vectors-server is running."
        : "The connection was lost before a response arrived. The request may still finish on the server; check its result before running it again.";
      throw Object.assign(new Error(message), { kind: "network" });
    }
    throw error;
  } finally {
    window.clearTimeout(timer);
    state.requests.delete(controller);
  }
}

function setConnection(status, label) {
  state.connection = status;
  const dot = $("#status-dot");
  dot.className = `status-dot ${status}`;
  $("#status-label").textContent = label;
  const banner = $("#connection-notice");
  banner.hidden = status === "online" || status === "connecting";
  const messages = {
    offline: "The server is unreachable. Your query and last loaded tables are kept here.",
    auth: "An API token is required to access this server.",
    busy: "The server is busy. Wait a moment, then retry your request.",
    warning: "The API connection needs attention. Your work is kept here.",
  };
  $("#connection-message").textContent = messages[status] || "";
}

function toast(message, kind = "success") {
  const item = node("div", `toast ${kind === "error" ? "error" : ""}`, message);
  $("#toast-region").append(item);
  window.setTimeout(() => item.remove(), 4200);
}

function showError(error, { quiet = false } = {}) {
  if (error.stale) return;
  if (error.status === 401) {
    setConnection("auth", "Token required");
    if (!quiet && !$("#token-dialog").open) $("#token-dialog").showModal();
  } else if (error.status === 503 || error.status === 429) {
    setConnection("busy", "Server busy");
  } else if (error.kind === "network") {
    setConnection("offline", "Disconnected");
  } else if (error.kind === "timeout" || error.kind === "protocol") {
    setConnection("warning", error.kind === "timeout" ? "Request timed out" : "Unexpected response");
  } else if (error.status) {
    // A rejected query still proves that the API is reachable.
    setConnection("online", "Connected");
  }
  if (!quiet) toast(error.status === 401 ? "Authentication required. Add the server API token." : error.message || String(error), "error");
}

function invalidateSchemas() {
  state.schemaGeneration += 1;
  state.schemas.clear();
  state.schemaRequests.clear();
}

function loadSchema(tableName) {
  if (state.schemas.has(tableName)) return Promise.resolve(state.schemas.get(tableName));
  if (state.schemaRequests.has(tableName)) return state.schemaRequests.get(tableName);
  const generation = state.schemaGeneration;
  const pending = request(`/v1/tables/${encodeURIComponent(tableName)}/schema`).then((schema) => {
    if (generation !== state.schemaGeneration) throw staleRequest();
    state.schemas.set(tableName, schema.columns);
    return schema.columns;
  }).finally(() => {
    if (state.schemaRequests.get(tableName) === pending) state.schemaRequests.delete(tableName);
  });
  state.schemaRequests.set(tableName, pending);
  return pending;
}

function loadTables({ quiet = false, force = false } = {}) {
  if (state.tablesRequest && !force) return state.tablesRequest;
  const generation = ++state.tablesGeneration;
  const recovering = state.connection === "offline" || state.connection === "warning";
  const started = performance.now();
  const pending = (async () => {
    try {
      if (!quiet) setConnection("connecting", "Connecting");
      const data = await request("/v1/tables");
      if (generation !== state.tablesGeneration) return null;
      const changed = force || recovering || state.revision !== data.revision;
      const tablesChanged = JSON.stringify(state.tables) !== JSON.stringify(data.tables);
      if (changed) invalidateSchemas();
      state.revision = data.revision;
      state.tables = data.tables;
      if (state.activeTable && !state.tables.some((table) => table.name === state.activeTable)) {
        state.activeTable = null;
        state.inspectorGeneration += 1;
        clear($("#table-inspector")).append(node("div", "empty-inspector", "Select a table to inspect its columns."));
      }
      if (tablesChanged || force) renderTableList();
      updateStats(data);
      if (tablesChanged || force) { populateSearchTables(); populateAdminTables(); }
      setConnection("online", "Connected");
      $("#connection-latency").textContent = `API ${Math.round(performance.now() - started)} ms`;
      const selected = $("#search-table").value;
      if (changed || state.searchColumnsFailed) void populateSearchColumns(selected);
      if (state.activeTable && (changed || state.inspectorFailed)) {
        void inspectTable(state.activeTable, { navigate: false });
      }
      return data;
    } catch (error) {
      if (generation === state.tablesGeneration && !error.stale) showError(error, { quiet });
      return null;
    } finally {
      if (generation === state.tablesGeneration) state.tablesRequest = null;
    }
  })();
  state.tablesRequest = pending;
  return pending;
}

async function refreshConnection({ quiet = false, force = false } = {}) {
  const health = request("/healthz").then((data) => {
    $("#app-version").textContent = data.version;
    $("#storage-mode-label").textContent = data.storage === "durable" ? "WAL + checkpoint" : "in memory";
  }).catch((error) => { if (!error.stale) $("#storage-mode-label").textContent = "storage unavailable"; });
  await Promise.all([health, loadTables({ quiet, force })]);
}

function updateStats(data) {
  const tables = data.tables || [];
  $("#stat-tables").textContent = tables.length.toLocaleString();
  $("#stat-rows").textContent = tables.reduce((sum, table) => sum + table.row_count, 0).toLocaleString();
  $("#stat-indexes").textContent = tables.reduce((sum, table) => sum + table.index_count, 0).toLocaleString();
  $("#stat-revision").textContent = data.revision ?? "—";
}

function renderTableList() {
  const list = clear($("#table-list"));
  if (!state.tables.length) {
    list.append(node("div", "sidebar-empty", "No tables yet. Create one in Data."));
    return;
  }
  state.tables.forEach((table) => {
    const button = node("button", `table-button ${state.activeTable === table.name ? "active" : ""}`);
    button.type = "button";
    button.dataset.table = table.name;
    button.disabled = state.admin.busy;
    button.append(node("span", "table-glyph", "▦"));
    button.append(node("span", "table-name", table.name));
    button.append(node("small", "", table.row_count.toLocaleString()));
    button.addEventListener("click", () => {
      if (state.view === "console") void inspectTable(table.name);
      else { switchView("data"); void selectAdminTable(table.name); }
    });
    list.append(button);
  });
}

async function inspectTable(tableName, { navigate = true } = {}) {
  const generation = ++state.inspectorGeneration;
  state.inspectorFailed = false;
  const schemaGeneration = state.schemaGeneration;
  state.activeTable = tableName;
  renderTableList();
  if (navigate) switchView("console");
  const inspector = clear($("#table-inspector"));
  const loading = node("div", "empty-inspector");
  loading.append(node("div", "empty-icon", "···"), node("p", "", `Loading ${tableName}`));
  inspector.append(loading);
  try {
    const [columns, indexes] = await Promise.all([
      loadSchema(tableName),
      request(`/v1/tables/${encodeURIComponent(tableName)}/indexes`),
    ]);
    if (generation !== state.inspectorGeneration || schemaGeneration !== state.schemaGeneration) return;
    renderInspector(tableName, columns, indexes.indexes);
  } catch (error) {
    if (generation !== state.inspectorGeneration || error.stale) return;
    state.inspectorFailed = true;
    clear(inspector).append(node("div", "empty-inspector", error.message));
    showError(error);
  }
}

function renderInspector(tableName, columns, indexes) {
  const inspector = clear($("#table-inspector"));
  const content = node("div", "inspector-content");
  const title = node("div", "inspector-title");
  title.append(node("span", "panel-kicker", "TABLE"), node("h3", "", tableName));
  const vectorCount = columns.filter((column) => column.data_type.startsWith("VECTOR"));
  title.append(node("p", "", `${columns.length} columns · ${vectorCount.length} vector · ${indexes.length} indexes`));
  content.append(title);

  const schemaList = node("div", "schema-list");
  columns.forEach((column) => {
    const row = node("div", "schema-row");
    const copy = node("div");
    copy.append(node("strong", "", column.name));
    const flags = [column.nullable ? "nullable" : "required", column.unique ? "unique" : null].filter(Boolean).join(" · ");
    copy.append(node("small", "", flags));
    row.append(copy, node("span", "type-pill", column.data_type));
    schemaList.append(row);
  });
  content.append(schemaList);

  const actions = node("div", "inspector-actions");
  const selectButton = node("button", "button ghost compact", "Select rows");
  selectButton.type = "button";
  selectButton.addEventListener("click", () => {
    setEditor(`SELECT *\nFROM ${quoteIdentifier(tableName)}\nLIMIT 100;`);
    $("#sql-editor").focus();
  });
  actions.append(selectButton);
  if (vectorCount.length) {
    const searchButton = node("button", "button primary compact", "Search vectors");
    searchButton.type = "button";
    searchButton.addEventListener("click", () => {
      switchView("search");
      $("#search-table").value = tableName;
      populateSearchColumns(tableName);
    });
    actions.append(searchButton);
  }
  content.append(actions);
  inspector.append(content);
}

function quoteIdentifier(value) {
  return `"${value.replaceAll('"', '""')}"`;
}

function switchView(view) {
  state.view = view;
  $$(".view").forEach((element) => element.classList.toggle("active", element.id === `view-${view}`));
  $$(".nav-item").forEach((element) => {
    const active = element.dataset.view === view;
    element.classList.toggle("active", active);
    if (active) element.setAttribute("aria-current", "page");
    else element.removeAttribute("aria-current");
  });
  const titles = { connections: "Connections", console: "SQL", search: "Search", data: "Data", settings: "Settings", guide: "Help" };
  $("#view-title").textContent = titles[view];
  setSidebarOpen(false);
  if (view === "settings") loadSettingsView();
  if (view === "connections" && !state.graph.busy) void loadGraphCollections();
  if (view === "data") {
    populateAdminTables();
    if (state.admin.table && !state.admin.rows.length && !state.admin.busy) void loadAdminRows();
    if (!state.relationships.busy) void loadRelationships();
  }
}

function setEditor(sql) {
  const editor = $("#sql-editor");
  editor.value = sql;
  updateLineNumbers();
}

function updateLineNumbers() {
  const lines = $("#sql-editor").value.split("\n").length;
  $("#line-numbers").textContent = Array.from({ length: lines }, (_, index) => index + 1).join("\n");
}

function setSqlBusy(busy, label) {
  state.sqlBusy = busy;
  $("#run-sql").disabled = busy;
  $("#analyze-sql").disabled = busy;
  $("#sql-results").setAttribute("aria-busy", String(busy));
  if (label) $("#editor-status").textContent = label;
}

async function runSql() {
  if (state.sqlBusy) return;
  const sql = $("#sql-editor").value.trim();
  if (!sql) { toast("Write or load a SQL statement first.", "error"); return; }
  const session = state.session;
  const started = performance.now();
  setSqlBusy(true, "Running…");
  try {
    const data = await request("/v1/sql", { method: "POST", body: JSON.stringify({ sql }) });
    renderSqlResults(data.results, performance.now() - started);
    $("#editor-status").textContent = "Complete";
    setConnection("online", "Connected");
    if (data.results.some((result) => result.type === "command")) {
      void loadTables({ quiet: true, force: true });
    }
  } catch (error) {
    if (error.stale) return;
    $("#editor-status").textContent = "Error";
    renderResultError(error);
    showError(error);
  } finally {
    if (session === state.session) setSqlBusy(false);
  }
}

async function analyzeSql() {
  if (state.sqlBusy) return;
  const sql = $("#sql-editor").value.trim();
  if (!sql) { toast("Write or load a SELECT statement first.", "error"); return; }
  const session = state.session;
  setSqlBusy(true, "Understanding…");
  try {
    const intent = await request("/v1/sql/intent", { method: "POST", body: JSON.stringify({ sql }) });
    renderQueryIntent(intent);
    $("#editor-status").textContent = "Intent ready";
    setConnection("online", "Connected");
  } catch (error) {
    if (error.stale) return;
    $("#editor-status").textContent = "Error";
    renderResultError(error);
    showError(error);
  } finally {
    if (session === state.session) setSqlBusy(false);
  }
}

function renderQueryIntent(intent) {
  const target = clear($("#sql-results"));
  target.className = "";
  $("#results-title").textContent = "Query intent";
  const metrics = clear($("#query-metrics"));
  metrics.append(node("span", "metric-chip", intent.operation.toUpperCase()));
  if (intent.table) metrics.append(node("span", "metric-chip", intent.table));
  if (intent.distinct) metrics.append(node("span", "metric-chip", "DISTINCT"));
  if (intent.aggregation) metrics.append(node("span", "metric-chip", "AGGREGATE"));
  if (intent.vector_search?.optimized) metrics.append(node("span", "metric-chip", "VectorTopK"));

  const summary = node("div", "intent-summary");
  summary.append(node("span", "panel-kicker", "SCHEMA-AWARE INTERPRETATION"), node("h3", "", intent.summary));
  const details = node("div", "intent-details");
  if (intent.filter) details.append(node("span", "", `Filter · ${intent.filter}`));
  if (intent.group_by.length) details.append(node("span", "", `Group · ${intent.group_by.join(", ")}`));
  if (intent.having) details.append(node("span", "", `Having · ${intent.having}`));
  if (intent.order_by.length) details.append(node("span", "", `Order · ${intent.order_by.join(", ")}`));
  if (intent.limit !== null) details.append(node("span", "", `Limit · ${intent.limit}`));
  if (intent.vector_search) {
    details.append(node("span", "", `Embedding · ${intent.vector_search.column} (${intent.vector_search.dimensions}D)`));
    details.append(node("span", "", `Metric · ${intent.vector_search.metric}`));
  }
  summary.append(details);
  target.append(summary);
  target.append(renderDataTable(
    ["output", "source column", "type", "role"],
    intent.columns.map((column) => [
      column.output_name,
      column.source_column || "—",
      column.data_type || "computed",
      column.role,
    ]),
  ));
}

function renderSqlResults(results, elapsed) {
  const target = clear($("#sql-results"));
  target.className = "";
  $("#results-title").textContent = `${results.length} statement${results.length === 1 ? "" : "s"} completed`;
  const metrics = clear($("#query-metrics"));
  metrics.append(node("span", "metric-chip", `${elapsed.toFixed(1)} ms`));
  results.forEach((result) => target.append(renderResult(result)));
}

function renderResult(result) {
  const block = node("div", "result-block");
  if (result.type === "command") {
    const command = node("div", "command-result");
    command.append(node("b", "", result.tag), node("span", "", `${result.rows_affected} row(s) affected`));
    block.append(command);
    return block;
  }
  block.append(renderDataTable(result.columns, result.rows, result.schema));
  const meta = node("div", "result-meta");
  meta.append(node("span", "", `${result.row_count} row(s)`), node("span", "", `${result.rows_examined} examined`));
  block.append(meta);
  return block;
}

function renderDataTable(columns, rows, schema = null) {
  const result = node("div", "result-table");
  const wrap = node("div", "data-table-wrap");
  wrap.tabIndex = 0;
  wrap.setAttribute("role", "region");
  wrap.setAttribute("aria-label", "Scrollable query results");
  const table = node("table", "data-table");
  renderDataTable.nextId = (renderDataTable.nextId || 0) + 1;
  const tableId = `result-table-${renderDataTable.nextId}`;
  table.id = tableId;
  table.setAttribute("aria-label", "Query results");
  const head = document.createElement("thead");
  const headRow = document.createElement("tr");
  columns.forEach((column, index) => {
    const cell = node("th", "", column);
    cell.scope = "col";
    const dataType = schema?.[index]?.data_type;
    if (dataType) cell.append(node("small", "result-column-type", dataType));
    headRow.append(cell);
  });
  head.append(headRow);
  const body = document.createElement("tbody");
  table.append(head, body);
  wrap.append(table);

  const controls = node("nav", "result-pagination");
  controls.setAttribute("aria-label", "Result pages");
  const range = node("span", "result-page-range");
  range.setAttribute("aria-live", "polite");
  range.setAttribute("aria-atomic", "true");
  const sizeLabel = node("label", "result-page-size", "Rows per page ");
  const sizeSelect = node("select");
  sizeSelect.setAttribute("aria-label", "Rows per page");
  [50, 100, 250].forEach((size) => {
    const option = node("option", "", String(size));
    option.value = String(size);
    sizeSelect.append(option);
  });
  sizeSelect.value = String(state.preferences.pageSize);
  sizeSelect.disabled = rows.length === 0;
  sizeLabel.append(sizeSelect);
  const pageButtons = node("div", "result-page-buttons");
  let page = 0;
  let pageSize = state.preferences.pageSize;
  const makePageButton = (label, text, destination) => {
    const button = node("button", "button ghost compact", text);
    button.type = "button";
    button.setAttribute("aria-label", label);
    button.setAttribute("aria-controls", tableId);
    button.addEventListener("click", () => {
      page = destination();
      renderPage();
    });
    pageButtons.append(button);
    return button;
  };
  const lastPage = () => Math.max(0, Math.ceil(rows.length / pageSize) - 1);
  const firstButton = makePageButton("First page", "First", () => 0);
  const previousButton = makePageButton("Previous page", "Previous", () => Math.max(0, page - 1));
  const nextButton = makePageButton("Next page", "Next", () => Math.min(lastPage(), page + 1));
  const lastButton = makePageButton("Last page", "Last", lastPage);

  function renderPage() {
    const start = page * pageSize;
    const end = Math.min(start + pageSize, rows.length);
    const fragment = document.createDocumentFragment();
    for (let rowIndex = start; rowIndex < end; rowIndex += 1) {
      const tableRow = document.createElement("tr");
      rows[rowIndex].forEach((value, columnIndex) => {
        tableRow.append(renderDataCell(value, columns[columnIndex], rowIndex + 1,
          `${tableId}-row-${rowIndex}-column-${columnIndex}`));
      });
      fragment.append(tableRow);
    }
    if (!rows.length) {
      const emptyRow = document.createElement("tr");
      const emptyCell = node("td", "empty-table-message", "No rows returned");
      emptyCell.colSpan = Math.max(1, columns.length);
      emptyRow.append(emptyCell);
      fragment.append(emptyRow);
    }
    body.replaceChildren(fragment);
    range.textContent = rows.length
      ? `Rows ${(start + 1).toLocaleString()}–${end.toLocaleString()} of ${rows.length.toLocaleString()}`
      : "No rows returned";
    firstButton.disabled = previousButton.disabled = page === 0;
    nextButton.disabled = lastButton.disabled = page === lastPage();
    wrap.scrollTop = 0;
  }

  sizeSelect.addEventListener("change", () => {
    const firstVisibleRow = page * pageSize;
    pageSize = Number(sizeSelect.value);
    page = Math.floor(firstVisibleRow / pageSize);
    renderPage();
  });
  controls.append(range, sizeLabel, pageButtons);
  result.append(wrap, controls);
  renderPage();
  return result;
}

function renderDataCell(value, column, rowNumber, id) {
  const cell = document.createElement("td");
  if (value === null) {
    cell.className = "null-value";
    cell.textContent = "NULL";
    return cell;
  }

  const previewLimit = 240;
  let preview;
  let expandable = false;
  if (Array.isArray(value)) {
    cell.className = "vector-value";
    const sample = value.slice(0, 10);
    expandable = value.length > sample.length;
    const parts = sample.map((item) => {
      if (item !== null && typeof item === "object") {
        expandable = true;
        return Array.isArray(item) ? "[…]" : "{…}";
      }
      const text = String(item);
      if (text.length > previewLimit) expandable = true;
      return text.slice(0, previewLimit);
    });
    preview = `[${parts.join(", ")}${value.length > sample.length ? ", …" : ""}]`;
  } else if (typeof value === "object") {
    preview = "{…}";
    expandable = true;
  } else {
    preview = String(value);
  }
  if (preview.length > previewLimit) {
    preview = `${preview.slice(0, previewLimit)}…`;
    expandable = true;
  }
  if (!expandable) {
    cell.textContent = preview;
    return cell;
  }

  const summary = node("span", "cell-value-preview", preview);
  const full = node("pre", "cell-value-full");
  full.id = `${id}-value`;
  full.hidden = true;
  const toggle = node("button", "cell-value-toggle", "Show full value");
  toggle.type = "button";
  toggle.setAttribute("aria-controls", full.id);
  toggle.setAttribute("aria-expanded", "false");
  const description = `for ${column}, row ${rowNumber}`;
  toggle.setAttribute("aria-label", `Show full value ${description}`);
  toggle.addEventListener("click", () => {
    const expanded = toggle.getAttribute("aria-expanded") !== "true";
    full.hidden = !expanded;
    summary.hidden = expanded;
    // Keep large values out of the DOM and avoid serializing vectors until requested.
    full.textContent = expanded
      ? (typeof value === "object" ? JSON.stringify(value) : String(value))
      : "";
    toggle.textContent = expanded ? "Collapse value" : "Show full value";
    toggle.setAttribute("aria-expanded", String(expanded));
    toggle.setAttribute("aria-label", `${toggle.textContent} ${description}`);
  });
  cell.append(summary, full, toggle);
  return cell;
}

function renderResultError(error) {
  const target = clear($("#sql-results"));
  target.className = "results-empty";
  const placeholder = node("div", "result-placeholder");
  placeholder.append(node("span", "", "!"), node("p", "", error.message));
  target.append(placeholder);
  $("#results-title").textContent = "Query failed";
  clear($("#query-metrics"));
}

function populateSearchTables() {
  const select = $("#search-table");
  const selected = select.value;
  clear(select).append(node("option", "", "Choose a table"));
  select.firstElementChild.value = "";
  state.tables.forEach((table) => {
    const option = node("option", "", table.name);
    option.value = table.name;
    select.append(option);
  });
  if (state.tables.some((table) => table.name === selected)) select.value = selected;
}

async function populateSearchColumns(tableName) {
  const generation = ++state.searchColumnsGeneration;
  state.searchColumnsFailed = false;
  const vectorSelect = $("#search-vector-column");
  const filterSelect = $("#filter-column");
  const projection = $("#search-select");
  const sameTable = vectorSelect.dataset.table === tableName;
  vectorSelect.dataset.table = tableName;
  const resetChoices = () => {
    clear(vectorSelect).append(node("option", "", "Choose a vector"));
    vectorSelect.firstElementChild.value = "";
    clear(filterSelect).append(node("option", "", "No filter"));
    filterSelect.firstElementChild.value = "";
  };
  if (!sameTable || !tableName) {
    resetChoices();
    projection.value = "";
    updateDimensionHint();
  }
  if (!tableName) {
    vectorSelect.disabled = false;
    filterSelect.disabled = false;
    return;
  }
  vectorSelect.disabled = true;
  filterSelect.disabled = true;
  try {
    const columns = await loadSchema(tableName);
    if (generation !== state.searchColumnsGeneration || $("#search-table").value !== tableName) return;
    // Keep choices until the winning response arrives, including edits made
    // while a refresh is pending. Overlapping refreshes share one schema read.
    const previousVector = vectorSelect.value;
    const previousFilter = filterSelect.value;
    const previousProjection = projection.value;
    resetChoices();
    columns.forEach((column) => {
      const option = node("option", "", `${column.name} · ${column.data_type}`);
      option.value = column.name;
      option.dataset.type = column.data_type;
      (column.data_type.startsWith("VECTOR") ? vectorSelect : filterSelect).append(option);
    });
    const scalarNames = columns.filter((column) => !column.data_type.startsWith("VECTOR")).map((column) => column.name);
    const projectionValid = previousProjection.split(",").map((name) => name.trim()).filter(Boolean)
      .every((name) => columns.some((column) => column.name === name));
    projection.value = (sameTable || previousProjection) && projectionValid ? previousProjection : scalarNames.slice(0, 5).join(", ");
    if (Array.from(vectorSelect.options).some((option) => option.value === previousVector)) vectorSelect.value = previousVector;
    if (Array.from(filterSelect.options).some((option) => option.value === previousFilter)) filterSelect.value = previousFilter;
    if (!vectorSelect.value && vectorSelect.options.length === 2) vectorSelect.selectedIndex = 1;
    updateDimensionHint();
  } catch (error) {
    if (generation === state.searchColumnsGeneration && !error.stale) {
      state.searchColumnsFailed = true;
      showError(error);
    }
  } finally {
    if (generation === state.searchColumnsGeneration) {
      vectorSelect.disabled = false;
      filterSelect.disabled = false;
    }
  }
}

function updateDimensionHint() {
  updateProviderNotes();
  const option = $("#search-vector-column").selectedOptions[0];
  const match = option?.dataset.type?.match(/VECTOR\((\d+)\)/);
  if (!match) {
    $("#dimension-hint").textContent = "Select a vector column to see its dimensions.";
    return;
  }
  const dimensions = Number(match[1]);
  $("#dimension-hint").textContent = `Expected dimensions: ${dimensions}`;
  const vectorInput = $("#search-vector");
  if (!vectorInput.value.trim() && dimensions <= 32) {
    vectorInput.value = Array.from({ length: dimensions }, (_, index) => index === 0 ? "1" : "0").join(", ");
  }
}

function parseFilterValue(value, dataType) {
  if (dataType === "BOOLEAN") {
    if (!/^(true|false)$/i.test(value.trim())) throw new Error("Boolean filter value must be true or false.");
    return value.trim().toLowerCase() === "true";
  }
  if (dataType === "INTEGER" || dataType === "DOUBLE") {
    const number = Number(value);
    if (!/^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?$/i.test(value.trim()) || !Number.isFinite(number)) throw new Error(`Filter value must be numeric for ${dataType}.`);
    if (dataType === "INTEGER" && (!/^[+-]?\d+$/.test(value.trim()) || !Number.isSafeInteger(number))) throw new Error("Integer filter value must be a safe whole number.");
    return number;
  }
  return value;
}

async function runVectorSearch(event) {
  event.preventDefault();
  if (state.searchBusy) return;
  const session = state.session;
  const generation = state.searchIntentGeneration;
  const button = $("#search-form button[type=submit]");
  try {
    const table = $("#search-table").value;
    const vectorColumn = $("#search-vector-column").value;
    if (!table || !vectorColumn || $("#search-vector-column").disabled) throw new Error("Choose a table and vector column first.");
    const match = $("#search-vector-column").selectedOptions[0]?.dataset.type?.match(/VECTOR\((\d+)\)/);
    if (!match) throw new Error("Select a valid vector column.");
    const dimensions = Number(match[1]);
    const limit = Number($("#search-limit").value);
    const maximum = Math.min(1000, state.serverSettings?.limits?.max_search_limit || 1000);
    if (!Number.isInteger(limit) || limit < 1 || limit > maximum) throw new Error(`Search limit must be a whole number between 1 and ${maximum}.`);
    const payload = {
      table, vector_column: vectorColumn, query: [], limit,
      metric: $("#search-metric").value,
      select: $("#search-select").value.split(",").map((value) => value.trim()).filter(Boolean),
      filters: [],
    };
    const filterColumn = $("#filter-column").value;
    if (filterColumn) payload.filters.push({
      column: filterColumn, operator: $("#filter-operator").value,
      value: parseFilterValue($("#filter-value").value, $("#filter-column").selectedOptions[0].dataset.type),
    });
    const mode = state.searchMode;
    const text = $("#search-text").value.trim();
    if (mode === "vector") {
      const values = $("#search-vector").value.split(",").map((value) => value.trim());
      payload.query = values.map(Number);
      if (values.some((value) => !value) || payload.query.some((value) => !Number.isFinite(Math.fround(value)))) throw new Error("Query vector must contain finite comma-separated numbers, with no empty values.");
      if (payload.query.length !== dimensions) throw new Error(`Query vector must contain ${dimensions} dimensions; received ${payload.query.length}.`);
    } else validateEmbeddingInputs([text]);
    state.searchBusy = true;
    button.disabled = true;
    $("#search-results").setAttribute("aria-busy", "true");
    const started = performance.now();
    let provenance = null;
    if (mode === "text") {
      const config = await embeddingConfigFor(dimensions);
      if (generation !== state.searchIntentGeneration) throw staleRequest();
      $("#search-results-title").textContent = "Embedding your query…";
      const generated = await generateEmbeddings([text], "query", config);
      if (generation !== state.searchIntentGeneration) throw staleRequest();
      payload.query = generated.embeddings[0];
      provenance = generated;
    }
    $("#search-results-title").textContent = "Searching…";
    const result = await request("/v1/vector/search", { method: "POST", body: JSON.stringify(payload) });
    if (generation !== state.searchIntentGeneration) throw staleRequest();
    renderSearchResult(result, performance.now() - started);
    if (provenance) $("#search-results").append(node("p", "result-provenance", `${embeddingDescription(provenance)} · ${provenance.usage?.total_tokens ?? 0} tokens`));
    setConnection("online", "Connected");
  } catch (error) {
    if (error.stale) {
      if (session === state.session) $("#search-results-title").textContent = "Search selection changed. Run again when ready.";
      return;
    }
    const target = clear($("#search-results"));
    target.className = "results-empty";
    target.append(node("p", "search-error", error.message));
    $("#search-results-title").textContent = "Search failed";
    showError(error);
  } finally {
    if (session === state.session) {
      state.searchBusy = false;
      button.disabled = false;
      $("#search-results").setAttribute("aria-busy", "false");
    }
  }
}

function renderSearchResult(result, elapsed) {
  const target = clear($("#search-results"));
  target.className = "";
  target.append(renderDataTable(result.columns, result.rows));
  const meta = node("div", "result-meta");
  meta.append(
    node("span", "", `${result.row_count} neighbor(s)`),
    node("span", "", `${result.rows_examined} examined`),
    node("span", "", `${elapsed.toFixed(1)} ms`),
  );
  target.append(meta);
  $("#search-results-title").textContent = result.row_count ? "Ranked by distance" : "No matching rows";
}

function readBrowserPreferences() {
  const defaults = { pageSize: 100, refreshSeconds: 15, compact: false };
  try {
    const saved = JSON.parse(localStorage.getItem("vectors.browser.preferences.v1") || "null");
    if (!saved || typeof saved !== "object") return defaults;
    return {
      pageSize: [50, 100, 250].includes(saved.pageSize) ? saved.pageSize : defaults.pageSize,
      refreshSeconds: [0, 15, 30, 60].includes(saved.refreshSeconds) ? saved.refreshSeconds : defaults.refreshSeconds,
      compact: saved.compact === true,
    };
  } catch { return defaults; }
}

function setSearchMode(mode) {
  state.searchMode = mode;
  state.searchIntentGeneration += 1;
  $("#search-mode-text").setAttribute("aria-pressed", String(mode === "text"));
  $("#search-mode-vector").setAttribute("aria-pressed", String(mode === "vector"));
  $("#search-text-fields").hidden = mode !== "text";
  $("#search-vector-fields").hidden = mode !== "vector";
  $("#search-text").required = mode === "text";
  $("#search-vector").required = mode === "vector";
  updateProviderNotes();
}

function embeddingDescription(settings) {
  if (!settings) return "Embedding settings unavailable. Open Settings to reconnect.";
  const label = settings.providers?.find((provider) => provider.id === settings.provider)?.label || settings.provider;
  return `${label} · ${settings.model} · ${settings.dimensions.toLocaleString()} dimensions`;
}

function updateProviderNotes() {
  const settings = state.embeddings;
  const description = embeddingDescription(settings);
  const note = settings && !settings.configured ? `${description}. Add an API key in Settings to use text search.` : description;
  $("#search-provider-note").textContent = note;
  $("#admin-provider-note").textContent = note;
  updateGraphCreateProfile();
  const match = $("#search-vector-column").selectedOptions[0]?.dataset.type?.match(/VECTOR\((\d+)\)/);
  if (state.searchMode === "text" && match && settings && Number(match[1]) !== settings.dimensions) {
    $("#search-provider-note").textContent = `${description}. This column needs ${match[1]} dimensions; update Settings or choose another column.`;
  }
}

async function loadEmbeddingSettings({ form = false, force = false } = {}) {
  if (state.embeddingRequest && !force) return state.embeddingRequest;
  const generation = ++state.embeddingGeneration;
  const pending = request("/v1/settings/embeddings").then((settings) => {
    if (generation !== state.embeddingGeneration) throw staleRequest();
    state.embeddings = settings;
    updateProviderNotes();
    if (form) renderEmbeddingSettings(settings);
    return settings;
  }).catch((error) => {
    if (!error.stale && generation === state.embeddingGeneration) {
      $("#search-provider-note").textContent = "Open Settings to connect an embedding provider, or switch to Vector mode.";
      $("#admin-provider-note").textContent = "Embedding settings unavailable. Open Settings to connect a provider.";
      if (form) {
        $("#embedding-settings-status").textContent = error.message;
        showError(error);
      }
    }
    throw error;
  }).finally(() => {
    if (state.embeddingRequest === pending) state.embeddingRequest = null;
  });
  state.embeddingRequest = pending;
  return pending;
}

function fillOptions(select, entries, selected, placeholder = null) {
  clear(select);
  if (placeholder !== null) {
    const empty = node("option", "", placeholder);
    empty.value = "";
    select.append(empty);
  }
  for (const entry of entries) {
    const option = node("option", "", entry.label || entry.id);
    option.value = entry.id;
    select.append(option);
  }
  if (Array.from(select.options).some((option) => option.value === selected)) select.value = selected;
}

function renderEmbeddingSettings(settings) {
  fillOptions($("#embedding-provider"), settings.providers, settings.provider);
  populateEmbeddingModels(settings.model, settings.dimensions);
  $("#embedding-batch-size").value = settings.batch_size;
  $("#embedding-timeout").value = settings.timeout_seconds;
  $("#embedding-concurrency").value = settings.max_concurrent_requests;
  $("#embedding-settings-status").textContent = settings.persistence === "durable"
    ? "Model and request settings persist across restarts. API keys stay in server memory."
    : "These settings last for this server session. API keys stay in server memory.";
}

function populateEmbeddingModels(selected = null, dimensions = null) {
  const provider = state.embeddings?.providers.find((item) => item.id === $("#embedding-provider").value);
  if (!provider) return;
  fillOptions($("#embedding-model"), provider.models, selected);
  updateEmbeddingDimensions(dimensions);
  $("#embedding-key-status").textContent = provider.configured
    ? `${provider.label} has an active API key. Leave the field blank to keep it.`
    : `No active ${provider.label} key. Enter one here or configure the server environment.`;
  $("#embedding-clear-key").checked = false;
}

function updateEmbeddingDimensions(value = null) {
  const provider = state.embeddings?.providers.find((item) => item.id === $("#embedding-provider").value);
  const model = provider?.models.find((item) => item.id === $("#embedding-model").value);
  if (!model) return;
  const dimensions = $("#embedding-dimensions");
  dimensions.value = value ?? model.default_dimensions ?? model.dimensions[0];
  dimensions.max = Math.max(...model.dimensions);
  $("#embedding-dimension-note").textContent = provider.id === "openai"
    ? `Choose 1–${dimensions.max} dimensions. This must match the vector column you use.`
    : `Supported dimensions: ${model.dimensions.join(", ")}. This must match your vector column.`;
}

async function saveEmbeddingSettings(event) {
  event.preventDefault();
  if (state.embeddingSaveBusy) return;
  const button = $("#embedding-save");
  const session = state.session;
  try {
    const provider = state.embeddings?.providers.find((item) => item.id === $("#embedding-provider").value);
    const model = provider?.models.find((item) => item.id === $("#embedding-model").value);
    if (!model) throw new Error("Load embedding settings before saving.");
    const dimensions = Number($("#embedding-dimensions").value);
    if (!Number.isInteger(dimensions) || dimensions < 1 || (provider.id === "openai"
      ? dimensions > Math.max(...model.dimensions) : !model.dimensions.includes(dimensions))) {
      throw new Error("Choose a supported dimension count for this model.");
    }
    const payload = {
      provider: provider.id, model: model.id, dimensions,
      batch_size: Number($("#embedding-batch-size").value),
      timeout_seconds: Number($("#embedding-timeout").value),
      max_concurrent_requests: Number($("#embedding-concurrency").value),
    };
    for (const [name, min, max] of [["batch_size", 1, 128], ["timeout_seconds", 1, 120], ["max_concurrent_requests", 1, 16]]) {
      if (!Number.isInteger(payload[name]) || payload[name] < min || payload[name] > max) throw new Error(`${name.replaceAll("_", " ")} must be between ${min} and ${max}.`);
    }
    const key = $("#embedding-api-key").value.trim();
    if (key && $("#embedding-clear-key").checked) throw new Error("Enter a replacement key or clear the current key, not both.");
    if (key) payload.api_key = key;
    if ($("#embedding-clear-key").checked) payload.clear_api_key = true;
    state.embeddingSaveBusy = true;
    button.disabled = true;
    $("#embedding-settings-status").textContent = "Saving…";
    const generation = ++state.embeddingGeneration;
    const settings = await request("/v1/settings/embeddings", { method: "PUT", body: JSON.stringify(payload) });
    if (generation !== state.embeddingGeneration) throw staleRequest();
    state.embeddings = settings;
    $("#embedding-api-key").value = "";
    $("#embedding-clear-key").checked = false;
    renderEmbeddingSettings(settings);
    updateProviderNotes();
    toast("Embedding settings saved.");
  } catch (error) {
    if (!error.stale) {
      $("#embedding-settings-status").textContent = error.message;
      showError(error);
    }
  } finally {
    if (session === state.session) { state.embeddingSaveBusy = false; button.disabled = false; }
  }
}

function validateEmbeddingInputs(input) {
  if (!Array.isArray(input) || !input.length || input.length > 256) throw new Error("Use between 1 and 256 documents per batch.");
  const encoder = new TextEncoder();
  let total = 0;
  for (const text of input) {
    if (typeof text !== "string" || !text.trim()) throw new Error("Every document must have nonempty text in the selected source column.");
    const size = encoder.encode(text).length;
    if (size > 32768) throw new Error("Each text must be at most 32 KiB. Split longer documents first.");
    total += size;
  }
  if (total > 1048576) throw new Error("This batch exceeds 1 MiB of text. Split it into smaller batches.");
}

async function embeddingConfigFor(dimensions) {
  const config = state.embeddings || await loadEmbeddingSettings();
  if (!config.configured) throw new Error("Add your embedding provider API key in Settings first.");
  if (config.dimensions !== dimensions) throw new Error(`The selected column needs ${dimensions} dimensions, but ${config.model} is set to ${config.dimensions}. Update Settings before generating embeddings.`);
  return config;
}

async function generateEmbeddings(input, inputType, config) {
  validateEmbeddingInputs(input);
  const expected = { provider: config.provider, model: config.model, dimensions: config.dimensions };
  const result = await request("/v1/embeddings", {
    method: "POST", timeout: Math.max(60000, (config.timeout_seconds + 10) * 1000),
    body: JSON.stringify({ input, input_type: inputType, expected_settings: expected }),
  });
  if (result.provider !== expected.provider || result.model !== expected.model || result.dimensions !== expected.dimensions
    || result.embeddings?.length !== input.length
    || result.embeddings.some((vector) => !Array.isArray(vector) || vector.length !== expected.dimensions || vector.some((value) => typeof value !== "number" || !Number.isFinite(Math.fround(value))))) {
    throw new Error("Embedding settings or dimensions changed. Refresh Settings before trying again.");
  }
  return result;
}

async function loadServerSettings() {
  try {
    const settings = await request("/v1/settings/server");
    state.serverSettings = settings;
    const target = clear($("#server-settings"));
    const groups = [
      ["Server", { version: settings.version, storage: settings.storage, authentication: settings.authentication ? "API token required" : "No API token" }],
      ["Compute", settings.compute], ["Limits", settings.limits], ["Capacity", settings.capacity],
    ];
    for (const [name, values] of groups) {
      const section = node("details", "runtime-group");
      if (name === "Server") section.open = true;
      section.append(node("summary", "", name));
      const list = node("dl");
      if (!values) list.append(node("dd", "", "Not reported by this server"));
      else for (const [key, value] of Object.entries(values)) {
        const line = node("div");
        line.append(node("dt", "", key.replaceAll("_", " ")), node("dd", "", typeof value === "boolean" ? (value ? "Enabled" : "Disabled") : String(value ?? "—")));
        list.append(line);
      }
      section.append(list);
      target.append(section);
    }
  } catch (error) {
    if (!error.stale) clear($("#server-settings")).append(node("p", "field-hint", error.message));
  }
}

function loadSettingsView() {
  if (!state.rerankingBusy) void loadRerankingSettings().catch(() => {});
  void loadEmbeddingSettings({ form: true, force: true }).catch(() => {});
  void loadServerSettings();
}

function applyBrowserPreferences() {
  document.body.classList.toggle("compact-rows", state.preferences.compact);
  $("#preference-page-size").value = String(state.preferences.pageSize);
  $("#preference-refresh").value = String(state.preferences.refreshSeconds);
  $("#preference-compact").checked = state.preferences.compact;
  window.clearInterval(state.refreshTimer);
  if (state.preferences.refreshSeconds) {
    state.refreshTimer = window.setInterval(() => {
      if (!document.hidden && state.connection !== "auth" && !state.sqlBusy && !state.searchBusy && !state.admin.busy) void loadTables({ quiet: true });
    }, state.preferences.refreshSeconds * 1000);
  }
}

function saveBrowserPreferences(event) {
  event.preventDefault();
  state.preferences = {
    pageSize: Number($("#preference-page-size").value),
    refreshSeconds: Number($("#preference-refresh").value), compact: $("#preference-compact").checked,
  };
  applyBrowserPreferences();
  try {
    localStorage.setItem("vectors.browser.preferences.v1", JSON.stringify(state.preferences));
    $("#preference-status").textContent = "Saved in this browser. Page size applies to newly loaded results.";
  } catch { $("#preference-status").textContent = "Applied for this session. Browser storage is unavailable."; }
}

function populateAdminTables() {
  const selected = state.admin.table;
  fillOptions($("#admin-table"), state.tables.map((table) => ({ id: table.name })), selected, "Choose a table");
  if (selected && !state.tables.some((table) => table.name === selected)) {
    state.admin.table = "";
    state.admin.generation += 1;
    state.admin.rows = [];
    state.admin.selected = null;
    $("#admin-tools").hidden = true;
    clear($("#admin-rows")).append(node("div", "empty-workspace", "This table no longer exists. Choose another table."));
  }
  $("#search-empty").hidden = state.tables.length !== 0;
  updateAdminButtons();
}

function updateAdminButtons() {
  const available = Boolean(state.admin.table);
  for (const id of ["admin-table", "admin-page-size", "admin-new-table", "admin-clear-selection", "admin-create-submit", "admin-drop-submit", "admin-delete-confirm", "admin-text-column", "admin-vector-column", "admin-clear-documents"]) $("#" + id).disabled = state.admin.busy;
  if (!state.admin.busy) $("#admin-drop-submit").disabled = $("#admin-drop-confirm").value !== state.admin.dropTarget?.table;
  $$(".table-button, [data-admin-edit-row]").forEach((button) => { button.disabled = state.admin.busy; });
  for (const id of ["admin-refresh", "admin-new-row", "admin-drop-table"]) $("#" + id).disabled = !available || state.admin.busy;
  $("#admin-previous").disabled = !available || state.admin.busy || state.admin.offset === 0;
  $("#admin-next").disabled = !available || state.admin.busy || state.admin.offset + state.admin.limit >= state.admin.total;
  for (const id of ["admin-add-row", "admin-save-row", "admin-delete-row", "admin-embed-insert"]) $("#" + id).disabled = !available || state.admin.busy;
  if (state.admin.selected && !state.admin.selected.key) {
    $("#admin-save-row").disabled = true;
    $("#admin-delete-row").disabled = true;
  }
  $("#admin-row-json").readOnly = state.admin.busy;
  $("#admin-documents-json").readOnly = state.admin.busy;
  const builder = fieldBuilders.get("admin-create-fields");
  if (builder) syncFieldBuilder(builder);
  $("#admin-create-name").disabled = state.admin.busy;
  $("#admin-create-schema").disabled = state.admin.busy;
  updateRelationshipControls();
}

async function selectAdminTable(tableName) {
  if (state.admin.busy) return;
  state.admin.table = tableName;
  state.admin.offset = 0;
  state.admin.selected = null;
  state.admin.generation += 1;
  state.activeTable = tableName || null;
  $("#admin-table").value = tableName;
  renderTableList();
  renderRelationships();
  $("#admin-tools").hidden = !tableName;
  updateAdminButtons();
  if (!tableName) {
    clear($("#admin-rows")).append(node("div", "empty-workspace", "Choose a table to browse its rows."));
    $("#admin-range").textContent = "0 rows";
    return;
  }
  await loadAdminRows();
}

async function loadAdminRows({ preserveEditor = false } = {}) {
  const tableName = state.admin.table;
  if (!tableName) return;
  const generation = ++state.admin.generation;
  const offset = state.admin.offset;
  $("#admin-status").textContent = "Loading rows…";
  $("#admin-rows").setAttribute("aria-busy", "true");
  try {
    const data = await request(`/v1/admin/tables/${encodeURIComponent(tableName)}/rows?limit=${state.admin.limit}&offset=${offset}`);
    if (generation !== state.admin.generation || tableName !== state.admin.table) return;
    state.admin.columns = data.columns;
    state.admin.schema = data.schema;
    state.admin.rows = data.rows;
    state.admin.total = data.total_rows;
    state.admin.revision = data.revision;
    state.admin.limit = data.limit;
    state.admin.offset = data.offset;
    if (!data.rows.length && data.total_rows > 0 && data.offset >= data.total_rows) {
      state.admin.offset = Math.floor((data.total_rows - 1) / data.limit) * data.limit;
      await loadAdminRows({ preserveEditor });
      return;
    }
    renderAdminRows();
    renderAdminSchema();
    if (!preserveEditor) resetRowEditor({ internal: true });
    $("#admin-status").textContent = `${tableName} · revision ${data.revision}`;
    $("#admin-tools").hidden = false;
    updateAdminButtons();
    setConnection("online", "Connected");
  } catch (error) {
    if (!error.stale && generation === state.admin.generation) {
      $("#admin-status").textContent = error.message;
      showError(error);
    }
  } finally {
    if (generation === state.admin.generation) $("#admin-rows").setAttribute("aria-busy", "false");
  }
}

function renderAdminRows() {
  const admin = state.admin;
  const container = clear($("#admin-rows"));
  const wrap = node("div", "data-table-wrap");
  wrap.tabIndex = 0;
  wrap.setAttribute("role", "region");
  wrap.setAttribute("aria-label", "Table rows");
  const table = node("table", "data-table");
  const head = node("thead");
  const headings = node("tr");
  ["Record", ...admin.columns].forEach((name) => {
    const cell = node("th", "", name);
    cell.scope = "col";
    headings.append(cell);
  });
  head.append(headings);
  const body = node("tbody");
  admin.rows.forEach((row, index) => {
    const tr = node("tr");
    const action = node("td");
    const button = node("button", "button ghost compact", "Edit");
    button.type = "button";
    button.setAttribute("aria-label", `Edit row ${admin.offset + index + 1}`);
    button.dataset.adminEditRow = String(index);
    button.disabled = admin.busy;
    button.addEventListener("click", () => selectAdminRow(index));
    action.append(button);
    tr.append(action);
    row.forEach((value, column) => tr.append(renderDataCell(value, admin.columns[column], admin.offset + index + 1, `admin-${admin.generation}-${index}-${column}`)));
    body.append(tr);
  });
  if (!admin.rows.length) {
    const row = node("tr");
    const cell = node("td", "empty-table-message", "This table is empty. Add a row or a document batch below.");
    cell.colSpan = admin.columns.length + 1;
    row.append(cell);
    body.append(row);
  }
  table.append(head, body);
  wrap.append(table);
  container.append(wrap);
  $("#admin-range").textContent = admin.total
    ? `Rows ${(admin.offset + 1).toLocaleString()}–${(admin.offset + admin.rows.length).toLocaleString()} of ${admin.total.toLocaleString()}`
    : "0 rows";
}

function renderAdminSchema() {
  const schema = state.admin.schema;
  const target = clear($("#admin-schema"));
  schema.forEach((column) => {
    const row = node("div", "schema-row");
    const label = node("div");
    label.append(node("strong", "", column.name), node("small", "", [column.nullable ? "nullable" : "required", column.unique ? "unique" : null].filter(Boolean).join(" · ")));
    row.append(label, node("span", "type-pill", column.data_type));
    target.append(row);
  });
  $("#admin-schema-summary").textContent = `${schema.length} columns`;
  const textSelect = $("#admin-text-column");
  const vectorSelect = $("#admin-vector-column");
  fillOptions(textSelect, schema.filter((column) => column.data_type === "TEXT").map((column) => ({ id: column.name })), textSelect.value, "Choose a text column");
  fillOptions(vectorSelect, schema.filter((column) => column.data_type.startsWith("VECTOR")).map((column) => ({ id: column.name, label: `${column.name} · ${column.data_type}` })), vectorSelect.value, "Choose a vector column");
  if (!textSelect.value) textSelect.value = schema.some((column) => column.name === "content" && column.data_type === "TEXT") ? "content" : (textSelect.options[1]?.value || "");
  if (!vectorSelect.value && vectorSelect.options.length === 2) vectorSelect.selectedIndex = 1;
  updateProviderNotes();
}

function resetRowEditor({ internal = false } = {}) {
  if (state.admin.busy && !internal) return;
  state.admin.selected = null;
  const record = Object.fromEntries(state.admin.schema.map((column) => [column.name,
    column.nullable ? null : column.data_type === "TEXT" ? "" : column.data_type === "BOOLEAN" ? false : column.data_type.startsWith("VECTOR") ? [] : 0]));
  $("#admin-row-json").value = JSON.stringify(record, null, 2);
  $("#admin-editor-title").textContent = "Add a row";
  $("#admin-key-note").textContent = "Enter a JSON object with the table's column names. Choose a new value for each unique column.";
  $("#admin-add-row").hidden = false;
  $("#admin-save-row").hidden = true;
  $("#admin-delete-row").hidden = true;
  $("#admin-edit-status").textContent = "";
  updateAdminButtons();
}

function selectAdminRow(index) {
  if (state.admin.busy) return;
  const row = state.admin.rows[index];
  const record = Object.fromEntries(state.admin.columns.map((name, column) => [name, row[column]]));
  const keyColumn = state.admin.schema.find((column) => column.unique && !column.data_type.startsWith("VECTOR")
    && record[column.name] !== null && !(column.data_type === "INTEGER" && !Number.isSafeInteger(record[column.name])));
  const key = keyColumn ? { column: keyColumn.name, value: record[keyColumn.name] } : null;
  state.admin.selected = { key, record, revision: state.admin.revision };
  $("#admin-row-json").value = JSON.stringify(record, null, 2);
  $("#admin-editor-title").textContent = `Edit row ${state.admin.offset + index + 1}`;
  $("#admin-key-note").textContent = key
    ? `Identified by ${key.column} = ${String(key.value)}. Changes use revision ${state.admin.revision}.`
    : "This row needs a non-null unique scalar key that the browser can represent exactly. Large integer IDs cannot be edited here; use SQL or another unique column.";
  $("#admin-add-row").hidden = true;
  $("#admin-save-row").hidden = false;
  $("#admin-delete-row").hidden = false;
  $("#admin-edit-status").textContent = "";
  updateAdminButtons();
  $("#admin-row-json").focus({ preventScroll: true });
  $("#admin-editor-title").scrollIntoView({ behavior: "smooth", block: "nearest" });
}

function parseRowObject(text) {
  let record;
  try { record = JSON.parse(text); } catch { throw new Error("Enter valid JSON for the row."); }
  if (!record || typeof record !== "object" || Array.isArray(record)) throw new Error("A row must be a JSON object keyed by column name.");
  return record;
}

function validateRecord(record, { generatedColumn = null, partial = false } = {}) {
  for (const name of Object.keys(record)) {
    if (!state.admin.schema.some((column) => column.name === name)) throw new Error(`Unknown column: ${name}.`);
  }
  for (const column of state.admin.schema) {
    if (column.name === generatedColumn || (partial && !Object.hasOwn(record, column.name))) continue;
    const value = record[column.name];
    if (value === null || value === undefined) {
      if (!column.nullable) throw new Error(`${column.name} is required.`);
      continue;
    }
    const type = column.data_type;
    if (type === "INTEGER" && !Number.isSafeInteger(value)) throw new Error(`${column.name} must be an integer within the browser's exact range.`);
    if (type === "DOUBLE" && (typeof value !== "number" || !Number.isFinite(value))) throw new Error(`${column.name} must be a finite number.`);
    if (type === "TEXT" && typeof value !== "string") throw new Error(`${column.name} must be text.`);
    if (type === "BOOLEAN" && typeof value !== "boolean") throw new Error(`${column.name} must be true or false.`);
    if (type.startsWith("VECTOR")) {
      const dimensions = Number(type.match(/VECTOR\((\d+)\)/)?.[1]);
      if (!Array.isArray(value) || value.length !== dimensions || value.some((part) => typeof part !== "number" || !Number.isFinite(Math.fround(part)))) throw new Error(`${column.name} must contain ${dimensions} finite numbers.`);
    }
  }
}

async function adminMutation(action, { statusId = "admin-edit-status", closeDialog = null } = {}) {
  if (state.admin.busy) return false;
  const session = state.session;
  state.admin.busy = true;
  updateAdminButtons();
  $("#" + statusId).textContent = "Saving…";
  try {
    const message = await action();
    if (closeDialog) $("#" + closeDialog).close();
    $("#" + statusId).textContent = message || "Saved.";
    toast(message || "Saved.");
    await Promise.all([loadTables({ quiet: true, force: true }), loadRelationships()]);
    if (session !== state.session) throw staleRequest();
    if (state.admin.table) await loadAdminRows();
    return true;
  } catch (error) {
    if (!error.stale) {
      $("#" + statusId).textContent = error.code === "stale_revision"
        ? "The data changed since you loaded it. Refresh rows, review the latest values, and try again."
        : error.message;
      showError(error);
    }
    return false;
  } finally {
    if (session === state.session) { state.admin.busy = false; updateAdminButtons(); }
  }
}

async function insertAdminRow() {
  const table = state.admin.table;
  await adminMutation(async () => {
    const record = parseRowObject($("#admin-row-json").value);
    validateRecord(record);
    await request(`/v1/tables/${encodeURIComponent(table)}/rows`, { method: "POST", body: JSON.stringify({ rows: [record] }) });
    return "Row inserted.";
  });
}

async function saveAdminRow() {
  const selected = state.admin.selected;
  const table = state.admin.table;
  if (!selected?.key) return;
  await adminMutation(async () => {
    const record = parseRowObject($("#admin-row-json").value);
    const values = Object.fromEntries(Object.entries(record).filter(([key, value]) => JSON.stringify(value) !== JSON.stringify(selected.record[key])));
    validateRecord(values, { partial: true });
    if (!Object.keys(values).length) return "No changes to save.";
    await request(`/v1/admin/tables/${encodeURIComponent(table)}/rows`, {
      method: "PATCH", body: JSON.stringify({ expected_revision: selected.revision, key: selected.key, values }),
    });
    return "Row updated.";
  });
}

function openDeleteRow() {
  const selected = state.admin.selected;
  if (!selected?.key || state.admin.busy) return;
  state.admin.deleteTarget = { table: state.admin.table, ...selected };
  $("#admin-delete-description").textContent = `Permanently delete ${selected.key.column} = ${String(selected.key.value)} from ${state.admin.table}?`;
  $("#admin-delete-status").textContent = "";
  $("#admin-delete-dialog").showModal();
}

async function deleteAdminRow(event) {
  event.preventDefault();
  const target = state.admin.deleteTarget;
  if (!target?.key) return;
  await adminMutation(async () => {
    await request(`/v1/admin/tables/${encodeURIComponent(target.table)}/rows`, {
      method: "DELETE", body: JSON.stringify({ expected_revision: target.revision, key: target.key }),
    });
    return "Row deleted.";
  }, { statusId: "admin-delete-status", closeDialog: "admin-delete-dialog" });
}

function relationshipPayload() {
  return {
    name: $("#relationship-name").value.trim().toLowerCase(),
    source_table: $("#relationship-source-table").value, source_column: $("#relationship-source-column").value,
    target_table: $("#relationship-target-table").value, target_column: $("#relationship-target-column").value,
  };
}

function duplicateRelationship(payload) {
  return state.relationships.items.some((item) => item.name.toLowerCase() === payload.name ||
    ["source_table", "source_column", "target_table", "target_column"].every((key) => item[key] === payload[key]));
}

function updateRelationshipControls() {
  const current = state.relationships;
  const editable = Number.isSafeInteger(current.revision) && current.revision >= 0 && !current.loading && !current.error && !state.admin.busy && !state.graph.busy;
  $("#relationship-new").disabled = current.busy || !editable || state.admin.busy || state.graph.busy;
  $("#relationships-refresh").disabled = current.busy;
  $$("#relationship-form input, #relationship-form select, #relationship-form button").forEach((control) => { control.disabled = current.busy; });
  $("#relationship-source-column").disabled = current.busy || current.schemaLoading || !current.sourceSchema.length;
  $("#relationship-target-column").disabled = current.busy || current.schemaLoading || !$("#relationship-source-column").value;
  const payload = relationshipPayload();
  const duplicate = Boolean(payload.name) && duplicateRelationship(payload);
  $("#relationship-save").disabled = current.busy || !editable || current.schemaLoading || duplicate;
  $$('[data-relationship-remove]').forEach((button) => { button.disabled = current.busy || !editable; });
}

function renderRelationships() {
  const current = state.relationships;
  const table = state.admin.table;
  const items = current.items.filter((item) => !table || item.source_table === table || item.target_table === table);
  const target = clear($("#relationship-list"));
  for (const item of items) {
    const card = node("article", "relationship-card"); card.dataset.relationshipName = item.name;
    const description = node("div");
    description.append(node("strong", "", item.name), node("p", "", `${item.source_table}.${item.source_column} → ${item.target_table}.${item.target_column}`));
    description.append(node("small", item.valid === false ? "relationship-error" : "", item.valid === false ? item.error || "This relationship no longer matches its table structure." : `${item.data_type} · matching values`));
    const actions = node("div", "inline-actions");
    const sql = node("button", "button ghost compact", "Open SQL"); sql.type = "button"; sql.dataset.relationshipSql = "";
    sql.disabled = item.valid === false;
    sql.addEventListener("click", () => openRelationshipSql(item));
    const remove = node("button", "button ghost compact", "Remove link"); remove.type = "button"; remove.dataset.relationshipRemove = "";
    remove.addEventListener("click", () => void removeRelationship(item));
    actions.append(sql, remove); card.append(description, actions); target.append(card);
  }
  if (!items.length && !current.error && !current.loading) target.append(node("p", "field-hint", table ? "No saved relationships for this table yet." : "Choose a table or connect two matching fields."));
  $("#relationships-status").textContent = [current.error || (current.loading ? "Loading relationships…" : `${items.length} ${items.length === 1 ? "relationship" : "relationships"}${table ? ` involving ${table}` : " across your tables"}.`), current.notice].filter(Boolean).join(" ");
  updateRelationshipControls();
}

async function loadRelationships() {
  const current = state.relationships;
  const generation = ++current.generation;
  current.loading = true; renderRelationships();
  try {
    const result = await request("/v1/relationships");
    if (generation !== state.relationships.generation) return false;
    if (!Array.isArray(result.relationships) || result.relationships.length > 256 || result.relationships.some((item) => !item || ["name", "source_table", "source_column", "target_table", "target_column", "data_type"].some((key) => typeof item[key] !== "string")) || !Number.isSafeInteger(result.revision) || result.revision < 0) throw new Error("The server returned an invalid relationship catalog. Refresh before making changes.");
    current.items = result.relationships; current.revision = result.revision; current.error = "";
    return true;
  } catch (error) {
    if (generation === state.relationships.generation && !error.stale) {
      current.revision = null;
      current.error = error.status === 404 ? "Relationships are unavailable on this server. Update the server to enable them." : `${error.message} Previously loaded links are kept here; refresh before making changes.`;
      if (error.status === 401) showError(error);
    }
    return false;
  } finally {
    if (generation === state.relationships.generation) { current.loading = false; renderRelationships(); }
  }
}

function openRelationshipSql(item) {
  const sql = `SELECT s.*, t.*\nFROM ${quoteIdentifier(item.source_table)} AS s\nLEFT JOIN ${quoteIdentifier(item.target_table)} AS t\n  ON s.${quoteIdentifier(item.source_column)} = t.${quoteIdentifier(item.target_column)}\nLIMIT 100;`;
  setEditor(sql); switchView("console"); $("#sql-editor").focus();
}

function openRelationshipDialog() {
  if (state.relationships.busy || state.relationships.error || !Number.isSafeInteger(state.relationships.revision)) return;
  $("#relationship-form").reset();
  const tables = state.tables.filter((table) => table.name !== "_vectors_relationships").map((table) => ({ id: table.name }));
  fillOptions($("#relationship-source-table"), tables, state.admin.table, "Choose a table");
  fillOptions($("#relationship-target-table"), tables, "", "Choose a table");
  $("#relationship-status").textContent = "";
  $("#relationship-dialog").showModal();
  void loadRelationshipColumns(); $("#relationship-name").focus();
}

function updateRelationshipTargets(selected = $("#relationship-target-column").value) {
  const current = state.relationships;
  const source = current.sourceSchema.find((column) => column.name === $("#relationship-source-column").value);
  const compatible = current.targetSchema.filter((column) => column.data_type === source?.data_type);
  fillOptions($("#relationship-target-column"), compatible.map((column) => ({ id: column.name, label: `${column.name} · ${column.data_type}` })), selected, "Choose a matching field");
  const payload = relationshipPayload();
  $("#relationship-status").textContent = payload.name && duplicateRelationship(payload) ? "A relationship with this name or these endpoints already exists."
    : source && $("#relationship-target-table").value && !compatible.length ? `The target table has no ${source.data_type} field. Choose another source field or target table.` : "";
  updateRelationshipControls();
}

async function loadRelationshipColumns() {
  const current = state.relationships;
  const generation = ++current.schemaGeneration;
  const source = $("#relationship-source-table").value; const target = $("#relationship-target-table").value;
  const selected = $("#relationship-source-column").value;
  const selectedTarget = $("#relationship-target-column").value;
  current.schemaLoading = true; current.sourceSchema = []; current.targetSchema = [];
  fillOptions($("#relationship-source-column"), [], "", "Loading fields…");
  fillOptions($("#relationship-target-column"), [], "", "Choose a matching field");
  updateRelationshipControls();
  try {
    const [sourceSchema, targetSchema] = await Promise.all([source ? loadSchema(source) : [], target ? loadSchema(target) : []]);
    if (generation !== state.relationships.schemaGeneration || !$("#relationship-dialog").open) return;
    const scalar = (column) => ["TEXT", "INTEGER", "DOUBLE", "BOOLEAN"].includes(column.data_type);
    current.sourceSchema = sourceSchema.filter(scalar); current.targetSchema = targetSchema.filter(scalar);
    fillOptions($("#relationship-source-column"), current.sourceSchema.map((column) => ({ id: column.name, label: `${column.name} · ${column.data_type}` })), selected, "Choose a scalar field");
    updateRelationshipTargets(selectedTarget);
  } catch (error) {
    if (!error.stale && generation === state.relationships.schemaGeneration) {
      $("#relationship-status").textContent = error.message; showError(error);
    }
  } finally {
    if (generation === state.relationships.schemaGeneration) { current.schemaLoading = false; updateRelationshipControls(); }
  }
}

async function relationshipMutation(action) {
  const current = state.relationships;
  if (current.busy) return;
  const session = state.session;
  current.busy = true; current.generation += 1; updateRelationshipControls();
  try {
    if (!Number.isSafeInteger(current.revision) || current.error || current.loading) throw new Error("Refresh relationships before making changes.");
    const expectedRevision = current.revision;
    const result = await action(expectedRevision);
    if (session !== state.session) throw staleRequest();
    // A link changes only its catalog and indexes. An exact pre-write row
    // snapshot remains valid; older snapshots still need their conflict guard.
    const admin = state.admin;
    if (admin.table && admin.table !== "_vectors_relationships" && admin.revision === expectedRevision && Number.isSafeInteger(result?.revision) && result.revision > expectedRevision) {
      admin.revision = result.revision;
      if ($("#admin-status").textContent === `${admin.table} · revision ${expectedRevision}`) $("#admin-status").textContent = `${admin.table} · revision ${result.revision}`;
      if (admin.selected?.revision === expectedRevision) {
        admin.selected.revision = result.revision;
        const key = admin.selected.key;
        if (key) $("#admin-key-note").textContent = `Identified by ${key.column} = ${String(key.value)}. Changes use revision ${result.revision}.`;
      }
    }
    await loadRelationships();
    void loadTables({ quiet: true, force: true });
  } catch (error) {
    if (!error.stale) {
      const message = error.code === "stale_revision" ? "The database changed. Your draft is preserved. Refresh links, review the latest values, then save again." : error.message;
      $("#relationship-status").textContent = message;
      current.error = message;
      current.revision = null;
      renderRelationships(); showError(error);
    }
  } finally {
    if (session === state.session) { current.busy = false; updateRelationshipControls(); }
  }
}

async function saveRelationship(event) {
  event.preventDefault();
  const payload = relationshipPayload();
  try {
    if (!/^[a-z][a-z0-9_]{0,47}$/.test(payload.name)) throw new Error("Use a relationship name with a lowercase letter first, then letters, digits, or underscores (up to 48 characters).");
    const source = state.relationships.sourceSchema.find((column) => column.name === payload.source_column);
    const target = state.relationships.targetSchema.find((column) => column.name === payload.target_column);
    if (state.relationships.schemaLoading || !payload.source_table || !payload.target_table || !source || !target || source.data_type !== target.data_type) throw new Error("Choose source and target fields with the same scalar type.");
    if (duplicateRelationship(payload)) throw new Error("A relationship with this name or these endpoints already exists.");
  } catch (error) { $("#relationship-status").textContent = error.message; return; }
  await relationshipMutation(async (revision) => {
    $("#relationship-status").textContent = "Saving relationship…";
    const result = await request("/v1/relationships", { method: "POST", body: JSON.stringify({ ...payload, expected_revision: revision }) });
    $("#relationship-dialog").close(); toast("Relationship saved. Open SQL to explore matching records.");
    return result;
  });
}

async function removeRelationship(item) {
  await relationshipMutation(async (revision) => {
    const result = await request(`/v1/relationships/${encodeURIComponent(item.name)}`, { method: "DELETE", body: JSON.stringify({ expected_revision: revision }) });
    toast("Link removed. The source and target records are unchanged.");
    return result;
  });
}

function resetRelationshipsSession() {
  const previous = state.relationships;
  state.relationships = { items: [], revision: null, generation: previous.generation + 1, schemaGeneration: previous.schemaGeneration + 1,
    busy: false, loading: false, error: "", sourceSchema: [], targetSchema: [],
    notice: previous.busy ? "The connection changed during a relationship request. It may still finish on the server; refresh and check before repeating it." : "" };
  $("#relationship-dialog").close(); $("#relationship-form").reset();
  fillOptions($("#relationship-source-table"), [], "", "Choose a table");
  fillOptions($("#relationship-target-table"), [], "", "Choose a table");
  fillOptions($("#relationship-source-column"), [], "", "Choose a scalar field");
  fillOptions($("#relationship-target-column"), [], "", "Choose a matching field");
  $("#relationship-status").textContent = "";
  renderRelationships();
}

const DOCUMENT_FIELD_RESERVED = new Set(["document_id", "title", "source", "text", "metadata", "chunking", "chunk_fingerprint"]);
const fieldBuilders = new Map();
let fieldRowId = 0;

function validateFieldDefinitions(columns, { documentFields = false } = {}) {
  const maximum = documentFields ? 32 : 256;
  if (!Array.isArray(columns) || columns.length > maximum || (!documentFields && !columns.length)) {
    throw new Error(`Choose ${documentFields ? "up to" : "between 1 and"} ${maximum} fields.`);
  }
  const names = new Set();
  return columns.map((column) => {
    if (!column || typeof column !== "object" || Array.isArray(column)) throw new Error("Every field must have a name and type.");
    const name = typeof column.name === "string" ? column.name.trim().toLowerCase() : "";
    if (!name || /[\u0000-\u001f\u007f]/.test(name) || new TextEncoder().encode(name).length > 128) throw new Error("Field names must contain 1–128 bytes without control characters.");
    if (documentFields && !/^[a-z][a-z0-9_]{0,47}$/.test(name)) throw new Error("Document field names need a letter first, then lowercase letters, digits, or underscores (up to 48 characters).");
    if (documentFields && DOCUMENT_FIELD_RESERVED.has(name)) throw new Error(`${name} is reserved for the document. Choose another field name.`);
    if (names.has(name)) throw new Error(`Duplicate field name: ${name}. Field names are case-insensitive.`);
    names.add(name);
    const data_type = typeof column.data_type === "string" ? column.data_type.trim().toUpperCase() : "";
    const vector = /^VECTOR\((\d+)\)$/.exec(data_type);
    if (!["TEXT", "INTEGER", "DOUBLE", "BOOLEAN"].includes(data_type) && (!vector || documentFields)) throw new Error(`${name} needs a ${documentFields ? "scalar " : ""}field type: TEXT, INTEGER, DOUBLE, or BOOLEAN${documentFields ? "." : ", or VECTOR(n)."}`);
    if (vector && (Number(vector[1]) < 1 || Number(vector[1]) > 65535)) throw new Error("Vector dimensions must be between 1 and 65,535.");
    if (column.nullable !== undefined && typeof column.nullable !== "boolean") throw new Error(`${name}: nullable must be true or false.`);
    if (column.unique !== undefined && typeof column.unique !== "boolean") throw new Error(`${name}: unique must be true or false.`);
    const nullable = column.nullable ?? true;
    const unique = column.unique ?? false;
    if (vector && unique) throw new Error(`${name}: vector fields cannot be unique.`);
    return { name, data_type: vector ? `VECTOR(${Number(vector[1])})` : data_type, nullable, unique };
  });
}

function mountFieldBuilder(id, columns = [], { documentFields = false } = {}) {
  const target = clear($("#" + id));
  const rows = node("div", "field-builder-rows");
  const empty = node("p", "field-hint", "No extra fields. You can store documents with their title, source, and text.");
  const add = node("button", "button ghost compact", "Add field");
  add.type = "button"; add.id = `${id}-add`;
  const builder = { id, target, rows, empty, add, documentFields };
  fieldBuilders.set(id, builder);
  target.append(rows, empty, add);
  add.addEventListener("click", () => { addFieldRow(builder); target.dataset.dirty = "true"; });
  for (const column of columns) addFieldRow(builder, column);
  syncFieldBuilder(builder);
}

function addFieldRow(builder, column = { name: "", data_type: "TEXT", nullable: true, unique: false }) {
  const row = node("div", "field-builder-row"); row.dataset.fieldRow = "";
  const number = ++fieldRowId;
  const name = node("input"); name.type = "text"; name.value = column.name; name.maxLength = builder.documentFields ? 48 : 128; name.placeholder = "e.g. category"; name.dataset.fieldName = "";
  const type = node("select"); type.dataset.fieldType = "";
  for (const choice of ["TEXT", "INTEGER", "DOUBLE", "BOOLEAN", ...(builder.documentFields ? [] : ["VECTOR"])]) {
    const option = node("option", "", choice); option.value = choice; type.append(option);
  }
  const vector = /^VECTOR\((\d+)\)$/.exec(column.data_type);
  type.value = vector ? "VECTOR" : column.data_type;
  const dimensions = node("input"); dimensions.type = "number"; dimensions.min = "1"; dimensions.max = "65535"; dimensions.value = vector?.[1] || state.embeddings?.dimensions || 1536; dimensions.dataset.fieldDimensions = "";
  if (builder.id === "admin-create-fields" && column.name === "embedding") dimensions.id = "admin-create-dimensions";
  const required = node("input"); required.type = "checkbox"; required.checked = !column.nullable; required.dataset.fieldRequired = "";
  const unique = node("input"); unique.type = "checkbox"; unique.checked = column.unique; unique.dataset.fieldUnique = "";
  const controlLabel = (text, control, className = "") => {
    const label = node("label", className, text);
    control.id ||= `${builder.id}-${number}-${text.replaceAll(" ", "-").toLowerCase()}`;
    label.htmlFor = control.id; label.append(control); return label;
  };
  const dimensionLabel = controlLabel("Vector dimensions", dimensions, "field-dimensions");
  const checks = node("div", "field-builder-checks"); checks.append(controlLabel("Required", required), controlLabel("Unique", unique));
  const remove = node("button", "icon-button field-remove", "×"); remove.type = "button"; remove.dataset.fieldRemove = ""; remove.setAttribute("aria-label", "Remove field");
  remove.addEventListener("click", () => { row.remove(); builder.target.dataset.dirty = "true"; syncFieldBuilder(builder); });
  row.append(controlLabel("Field name", name), controlLabel("Type", type), dimensionLabel, checks, remove);
  row.addEventListener("input", () => { builder.target.dataset.dirty = "true"; });
  type.addEventListener("change", () => { if (type.value === "VECTOR") unique.checked = false; syncFieldBuilder(builder); });
  builder.rows.append(row); syncFieldBuilder(builder);
}

function syncFieldBuilder(builder) {
  const busy = builder.documentFields ? state.graph.busy : state.admin.busy;
  const rows = $$('[data-field-row]', builder.rows);
  builder.empty.hidden = rows.length > 0 || !builder.documentFields;
  builder.add.disabled = busy || rows.length >= (builder.documentFields ? 32 : 256);
  for (const row of rows) {
    $$("input, select, button", row).forEach((element) => { element.disabled = busy; });
    const vector = $('[data-field-type]', row).value === "VECTOR";
    $(".field-dimensions", row).hidden = !vector;
    $('[data-field-dimensions]', row).disabled = busy || !vector;
    $('[data-field-unique]', row).disabled = busy || vector;
  }
}

function readFieldBuilder(id) {
  const builder = fieldBuilders.get(id);
  const columns = $$('[data-field-row]', builder.rows).map((row) => {
    const type = $('[data-field-type]', row).value;
    return { name: $('[data-field-name]', row).value, data_type: type === "VECTOR" ? `VECTOR(${$('[data-field-dimensions]', row).value})` : type,
      nullable: !$('[data-field-required]', row).checked, unique: $('[data-field-unique]', row).checked };
  });
  return validateFieldDefinitions(columns, builder);
}

function openCreateTable() {
  if (state.admin.busy || state.graph.busy) return;
  $("#data-create-dialog").close();
  $("#admin-create-status").textContent = "";
  if (!fieldBuilders.has("admin-create-fields")) {
    mountFieldBuilder("admin-create-fields", [
      { name: "id", data_type: "INTEGER", nullable: false, unique: true },
      { name: "title", data_type: "TEXT", nullable: true, unique: false },
      { name: "content", data_type: "TEXT", nullable: false, unique: false },
      { name: "embedding", data_type: `VECTOR(${state.embeddings?.dimensions || 1536})`, nullable: true, unique: false },
    ]);
  }
  $("#admin-create-dialog").showModal();
  $("#admin-create-name").focus();
}

async function createAdminTable(event) {
  event.preventDefault();
  if (state.admin.busy) return;
  const created = await adminMutation(async () => {
    const name = $("#admin-create-name").value.trim().toLowerCase();
    if (!name || new TextEncoder().encode(name).length > 128) throw new Error("Enter a table name of at most 128 bytes.");
    let columns;
    if ($("#admin-create-schema").value.trim()) {
      try { columns = JSON.parse($("#admin-create-schema").value); } catch { throw new Error("Enter valid JSON for the custom schema."); }
      columns = validateFieldDefinitions(columns);
    } else {
      columns = readFieldBuilder("admin-create-fields");
    }
    await request("/v1/admin/tables", { method: "POST", body: JSON.stringify({ name, columns }) });
    state.admin.table = name;
    state.admin.offset = 0;
    return `Created ${name}.`;
  }, { statusId: "admin-create-status", closeDialog: "admin-create-dialog" });
  if (created) switchView("data");
}

function openDropTable() {
  if (!state.admin.table || state.admin.busy) return;
  state.admin.dropTarget = { table: state.admin.table, revision: state.admin.revision };
  $("#admin-drop-title").textContent = `Delete ${state.admin.table}?`;
  $("#admin-drop-confirm").value = "";
  $("#admin-drop-status").textContent = "";
  $("#admin-drop-submit").disabled = true;
  $("#admin-drop-dialog").showModal();
}

async function dropAdminTable(event) {
  event.preventDefault();
  const target = state.admin.dropTarget;
  if (!target || $("#admin-drop-confirm").value !== target.table) return;
  await adminMutation(async () => {
    await request(`/v1/admin/tables/${encodeURIComponent(target.table)}`, {
      method: "DELETE", body: JSON.stringify({ expected_revision: target.revision, confirm_table: target.table }),
    });
    state.admin.table = "";
    state.admin.selected = null;
    state.admin.generation += 1;
    $("#admin-tools").hidden = true;
    clear($("#admin-rows")).append(node("div", "empty-workspace", "Table deleted. Create or choose another table."));
    $("#admin-range").textContent = "0 rows";
    return `Deleted ${target.table}.`;
  }, { statusId: "admin-drop-status", closeDialog: "admin-drop-dialog" });
}

async function embedAndInsertDocuments() {
  const table = state.admin.table;
  const generation = state.admin.generation;
  await adminMutation(async () => {
    const source = $("#admin-text-column").value;
    const target = $("#admin-vector-column").value;
    const column = state.admin.schema.find((item) => item.name === target && item.data_type.startsWith("VECTOR"));
    if (!source || !column) throw new Error("Choose the text source and vector target columns first.");
    let rows;
    try { rows = JSON.parse($("#admin-documents-json").value); } catch { throw new Error("Enter a valid JSON array of document rows."); }
    if (!Array.isArray(rows) || !rows.length || rows.length > 256) throw new Error("Enter between 1 and 256 document row objects.");
    for (const row of rows) {
      if (!row || typeof row !== "object" || Array.isArray(row)) throw new Error("Every document must be a JSON row object.");
      if (row[target] !== undefined && row[target] !== null) throw new Error(`${target} already has a value. Remove it to generate a new embedding; existing embeddings are never overwritten automatically.`);
      validateRecord(row, { generatedColumn: target });
    }
    const input = rows.map((row) => row[source]);
    validateEmbeddingInputs(input);
    const dimensions = Number(column.data_type.match(/VECTOR\((\d+)\)/)?.[1]);
    const config = await embeddingConfigFor(dimensions);
    if (generation !== state.admin.generation || table !== state.admin.table) throw staleRequest();
    $("#admin-ingest-status").textContent = `Generating ${rows.length} embeddings with ${config.model}…`;
    const generated = await generateEmbeddings(input, "document", config);
    if (generation !== state.admin.generation || table !== state.admin.table) throw staleRequest();
    const completed = rows.map((row, index) => ({ ...row, [target]: generated.embeddings[index] }));
    $("#admin-ingest-status").textContent = "Embeddings ready. Inserting documents…";
    await request(`/v1/tables/${encodeURIComponent(table)}/rows`, { method: "POST", body: JSON.stringify({ rows: completed }) });
    $("#admin-documents-json").value = "";
    return `Inserted ${rows.length} documents · ${embeddingDescription(generated)} · ${generated.usage?.total_tokens ?? 0} tokens.`;
  }, { statusId: "admin-ingest-status" });
}

function newGraphState() {
  return { collections: [], collection: "", catalogGeneration: 0, generation: 0, previewGeneration: 0,
    searchGeneration: 0, seedExplicit: false, busy: false, searchBusy: false, loading: false, notice: "",
    filterDrafts: new Map(), filterSchema: "",
    mode: "pages", rootChunk: null, rootLabel: "", pageSelection: null, neighborhoodTruncated: false, offset: 0, limit: 100, total: 0,
    revision: null, nodes: [], edges: [], selected: null, zoom: 1, pan: { x: 0, y: 0 }, dragged: false };
}

const GRAPH_COLORS = ["#65d9e8", "#c8f560", "#b8a5ff", "#ff9e64", "#f6a6cf", "#8dafff", "#75d7b4"];
function graphColor(id) {
  let hash = 0;
  for (const character of String(id)) hash = ((hash * 31) + character.codePointAt(0)) >>> 0;
  return GRAPH_COLORS[hash % GRAPH_COLORS.length];
}
function graphPath(suffix = "") { return `/v1/graph/collections/${encodeURIComponent(state.graph.collection)}${suffix}`; }
function graphCollection() { return state.graph.collections.find((item) => item.config.name === state.graph.collection); }
function graphLabel(item) {
  const title = item.title || item.document_id || item.chunk_id;
  const citation = Number.isInteger(item.ordinal) ? `passage ${item.ordinal + 1}` : Number.isInteger(item.start_byte) && Number.isInteger(item.end_byte) ? `bytes ${item.start_byte}–${item.end_byte}` : "";
  return citation ? `${title} · ${citation}` : title;
}
function graphProfileNote() {
  const collection = graphCollection();
  $("#graph-profile").textContent = collection
    ? `${embeddingDescription(collection.config.profile)} · fixed collection profile`
    : "Collections keep a fixed embedding model for documents and questions.";
  for (const id of ["graph-view-data", "graph-open-sql", "graph-add-open"]) $("#" + id).disabled = !collection || state.graph.busy;
}

function updateGraphCreateProfile() {
  const settings = state.embeddings;
  $("#graph-create-profile").textContent = settings
    ? `${embeddingDescription(settings)} · ${settings.configured ? "provider ready" : "API key needed before embedding"}`
    : "Embedding settings unavailable. Open Settings before creating a collection.";
}

function openCreateCollection() {
  if (state.graph.busy || state.admin.busy) return;
  $("#data-create-dialog").close();
  $("#graph-create-status").textContent = "";
  updateGraphCreateProfile();
  if (!fieldBuilders.has("graph-create-fields")) mountFieldBuilder("graph-create-fields", [], { documentFields: true });
  $("#graph-create-dialog").showModal();
  $("#graph-create-name").focus();
}

function graphDocumentColumns() {
  return validateFieldDefinitions(graphCollection()?.document_columns || [], { documentFields: true });
}

const GRAPH_FILTER_OPERATORS = [
  ["eq", "Equals"], ["ne", "Does not equal"], ["lt", "Less than"], ["lte", "At most"],
  ["gt", "Greater than"], ["gte", "At least"], ["is_null", "Is empty (null)"], ["is_not_null", "Has a value"],
];
function graphFilterColumns() {
  return ["document_id", "title", "source"].map((name) => ({ name, data_type: "TEXT" })).concat(graphDocumentColumns());
}
function graphFilterDraft() {
  const drafts = state.graph.filterDrafts;
  if (!drafts.has(state.graph.collection)) drafts.set(state.graph.collection, []);
  return drafts.get(state.graph.collection);
}
function readGraphFilters() {
  const draft = graphFilterDraft();
  if (!draft.length) return [];
  if (!graphCollection()) throw new Error("Choose a collection before filtering its documents.");
  if (draft.length > 32) throw new Error("Use at most 32 document filters.");
  const columns = graphFilterColumns();
  return draft.map((item) => {
    if (!item.column) throw new Error("Choose a document field for each filter before retrieving context.");
    const column = columns.find((column) => column.name === item.column);
    if (!column || column.data_type !== item.dataType) throw new Error(`The field ${item.column} changed or is unavailable. Choose its field again or remove the filter.`);
    if (!GRAPH_FILTER_OPERATORS.some(([operator]) => operator === item.operator)) throw new Error("Choose a valid document filter condition.");
    if (item.operator === "is_null" || item.operator === "is_not_null") return { column: column.name, operator: item.operator === "is_null" ? "eq" : "ne", value: null };
    if (column.data_type === "BOOLEAN" && !["eq", "ne"].includes(item.operator)) throw new Error("Boolean fields support equals, does not equal, and empty value conditions.");
    return { column: column.name, operator: item.operator, value: parseFilterValue(item.value, column.data_type) };
  });
}
function describeGraphFilters(filters) {
  return filters.map((filter) => {
    if (filter.value === null) return `${filter.column} ${filter.operator === "eq" ? "is empty" : "has a value"}`;
    const label = GRAPH_FILTER_OPERATORS.find(([operator]) => operator === filter.operator)?.[1].toLowerCase() || filter.operator;
    const value = JSON.stringify(filter.value);
    return `${filter.column} ${label} ${value.length > 100 ? `${value.slice(0, 100)}…` : value}`;
  }).join(" AND ");
}
function graphFiltersChanged() {
  state.graph.searchGeneration += 1;
  state.graph.searchBusy = false;
  $("#graph-search-results").setAttribute("aria-busy", "false");
  clear($("#graph-search-results")).append(node("p", "empty-workspace", "Document filters changed. Retrieve context to see matching passages."));
  $("#graph-search-status").textContent = "";
  updateGraphFilterSummary(); updateGraphPaging();
}
function updateGraphFilterSummary() {
  const count = graphFilterDraft().length;
  $("#graph-filters-count").textContent = count ? ` · ${count}` : "";
  try {
    const filters = readGraphFilters();
    $("#graph-filter-summary").textContent = filters.length
      ? `Match all: ${describeGraphFilters(filters)}. Only matching documents can supply passages or connecting context.`
      : "All documents are eligible. Add filters to narrow the source context.";
  } catch (error) { $("#graph-filter-summary").textContent = error.message; }
}
function syncGraphFilters() {
  const disabled = state.graph.busy || state.graph.searchBusy;
  $("#graph-filter-add").disabled = disabled || !graphCollection() || graphFilterDraft().length >= 32 || Boolean($("#graph-document-filters").dataset.error);
  $("#graph-filter-clear").disabled = disabled || !graphFilterDraft().length;
  for (const row of $$("[data-graph-filter-row]")) {
    const noValue = ["is_null", "is_not_null"].includes($("[data-filter-operator]", row).value);
    $("[data-filter-value]", row).disabled = disabled || noValue;
  }
}
function renderGraphFilters() {
  const target = clear($("#graph-document-filters"));
  let columns;
  try { columns = graphFilterColumns(); target.dataset.error = ""; }
  catch (error) {
    columns = []; target.dataset.error = error.message;
    target.append(node("p", "inline-status", `Document fields could not be loaded: ${error.message}`));
  }
  const signature = JSON.stringify([state.graph.collection, columns]);
  if (state.graph.filterSchema && signature !== state.graph.filterSchema && graphFilterDraft().length) graphFiltersChanged();
  state.graph.filterSchema = signature;
  for (const [index, item] of graphFilterDraft().entries()) {
    const row = node("div", "graph-filter-row"); row.dataset.graphFilterRow = "";
    const label = (text, input) => { const element = node("label", "", text); element.append(input); return element; };
    const field = node("select"); field.dataset.filterColumn = "";
    fillOptions(field, columns.map((column) => ({ id: column.name, label: `${column.name} · ${column.data_type.toLowerCase()}` })), item.column, "Choose a field");
    if (columns.some((column) => column.name === item.column && column.data_type !== item.dataType)) {
      field.value = ""; field.firstElementChild.textContent = "Choose field again (type changed)";
    }
    if (item.column && !columns.some((column) => column.name === item.column)) {
      const missing = node("option", "", `${item.column} · unavailable`); missing.value = item.column; field.append(missing); field.value = item.column;
    }
    field.addEventListener("change", () => {
      item.column = field.value; item.dataType = columns.find((column) => column.name === item.column)?.data_type;
      item.value = ""; item.operator = "eq"; renderGraphFilters(); graphFiltersChanged();
      $$("[data-filter-column]", target)[index]?.focus();
    });
    const operator = node("select"); operator.dataset.filterOperator = "";
    const operators = GRAPH_FILTER_OPERATORS.filter(([value]) => item.dataType !== "BOOLEAN" || ["eq", "ne", "is_null", "is_not_null"].includes(value));
    fillOptions(operator, operators.map(([id, label]) => ({ id, label })), item.operator);
    operator.addEventListener("change", () => { item.operator = operator.value; graphFiltersChanged(); });
    const input = node(item.dataType === "BOOLEAN" ? "select" : "input"); input.dataset.filterValue = "";
    if (item.dataType === "BOOLEAN") fillOptions(input, [{ id: "true", label: "True" }, { id: "false", label: "False" }], item.value, "Choose true or false");
    else { input.type = "text"; input.value = item.value; if (item.dataType === "INTEGER" || item.dataType === "DOUBLE") input.inputMode = "decimal"; }
    input.addEventListener("input", () => { item.value = input.value; graphFiltersChanged(); });
    const remove = node("button", "button ghost compact graph-filter-remove", "Remove"); remove.type = "button";
    remove.setAttribute("aria-label", `Remove document filter ${index + 1}`);
    remove.addEventListener("click", () => { graphFilterDraft().splice(index, 1); renderGraphFilters(); graphFiltersChanged(); });
    row.append(label("Document field", field), label("Condition", operator), label("Value", input), remove); target.append(row);
  }
  updateGraphFilterSummary(); syncGraphFilters();
}

function renderGraphDocumentFields({ reset = false } = {}) {
  const target = $("#graph-document-fields");
  let columns;
  try { columns = graphDocumentColumns(); }
  catch (error) {
    clear(target); target.dataset.owner = "";
    $("#graph-document-fields-panel").hidden = false;
    target.append(node("p", "inline-status", `Document fields could not be loaded: ${error.message}`));
    return;
  }
  const owner = JSON.stringify([state.graph.collection, columns]);
  if (!reset && target.dataset.owner === owner) return;
  clear(target); target.dataset.owner = owner;
  $("#graph-document-fields-panel").hidden = !columns.length;
  for (const column of columns) {
    const item = node("div", "document-field");
    const label = node("label", "", column.name);
    const hint = node("small", "", `${column.data_type.toLowerCase()} · ${column.nullable ? "optional" : "required"}${column.unique ? " · unique" : ""}`);
    const input = node(column.data_type === "BOOLEAN" ? "select" : "input");
    input.dataset.documentField = column.name;
    input.id = `graph-document-field-${column.name}`;
    label.htmlFor = input.id;
    if (column.data_type === "BOOLEAN") {
      fillOptions(input, [{ id: "true", label: "True" }, { id: "false", label: "False" }], "", column.nullable ? "No value" : "Choose true or false");
    } else {
      input.type = column.data_type === "TEXT" ? "text" : "number";
      if (input.type === "number") input.step = column.data_type === "INTEGER" ? "1" : "any";
    }
    input.required = !column.nullable;
    label.append(hint, input); item.append(label);
    if (column.nullable) {
      const empty = node("label", "document-null", "No value (null)");
      const checkbox = node("input"); checkbox.type = "checkbox"; checkbox.dataset.documentNull = column.name;
      checkbox.addEventListener("change", syncGraphDocumentFields);
      empty.prepend(checkbox); item.append(empty);
    }
    target.append(item);
  }
  syncGraphDocumentFields();
}

function syncGraphDocumentFields() {
  for (const input of $$('[data-document-field]')) {
    const nullControl = $$('[data-document-null]').find((element) => element.dataset.documentNull === input.dataset.documentField);
    input.disabled = state.graph.busy || Boolean(nullControl?.checked);
    if (nullControl) nullControl.disabled = state.graph.busy;
  }
}

function readGraphDocumentFields() {
  const target = $("#graph-document-fields");
  const columns = graphDocumentColumns();
  if (target.dataset.owner !== JSON.stringify([state.graph.collection, columns])) throw new Error("Reload this collection before editing its document fields.");
  return Object.fromEntries(columns.map((column) => {
    const input = $$('[data-document-field]', target).find((element) => element.dataset.documentField === column.name);
    const nullControl = $$('[data-document-null]', target).find((element) => element.dataset.documentNull === column.name);
    if (!input) throw new Error("Reload this collection to load its document fields.");
    const raw = input.value;
    if (!nullControl?.checked && input.validity.badInput) throw new Error(`${column.name} must be a valid number, or explicitly choose no value.`);
    if (nullControl?.checked || !raw.trim()) {
      if (!column.nullable) throw new Error(`${column.name} is required.`);
      return [column.name, null];
    }
    let value = raw;
    if (column.data_type === "BOOLEAN") {
      if (!["true", "false"].includes(raw)) throw new Error(`${column.name} must be true or false.`);
      value = raw === "true";
    } else if (column.data_type === "INTEGER" || column.data_type === "DOUBLE") {
      value = Number(raw);
      if (!Number.isFinite(value)) throw new Error(`${column.name} must be a finite number.`);
      if (column.data_type === "INTEGER" && (!/^[+-]?\d+$/.test(raw) || !Number.isSafeInteger(value))) throw new Error(`${column.name} must be an integer represented exactly by this browser (at most 9,007,199,254,740,991 in magnitude).`);
    }
    return [column.name, value];
  }));
}

function graphTableName(collection, kind) {
  return collection.tables?.[kind] || `graph_${collection.config.name}_${kind}`;
}

async function viewGraphData() {
  const collection = graphCollection();
  if (!collection || state.graph.busy || state.admin.busy) return;
  const session = state.session;
  const name = collection.config.name;
  await loadTables({ quiet: true, force: true });
  if (session !== state.session || name !== state.graph.collection || state.graph.busy || state.admin.busy) return;
  const table = graphTableName(collection, "documents");
  switchView("data");
  await selectAdminTable(table);
}

function openGraphSql() {
  const collection = graphCollection();
  if (!collection || state.graph.busy) return;
  const fields = graphDocumentColumns();
  const qualified = (alias, name) => `${alias}.${quoteIdentifier(name)}`;
  const columns = [qualified("c", "chunk_id"), qualified("d", "document_id"), qualified("d", "title"), ...fields.map((column) => qualified("d", column.name)), qualified("c", "text")];
  const first = fields[0];
  const filter = first ? `-- Optional scalar filter before LIMIT:\n-- WHERE ${qualified("d", first.name)} = ${{ TEXT: "'value'", INTEGER: "0", DOUBLE: "0.0", BOOLEAN: "TRUE" }[first.data_type]}\n` : "";
  const sql = `SELECT ${columns.join(",\n       ")}\nFROM ${quoteIdentifier(graphTableName(collection, "chunks"))} AS c\nJOIN ${quoteIdentifier(graphTableName(collection, "documents"))} AS d\n  ON c.${quoteIdentifier("document_id")} = d.${quoteIdentifier("document_id")}\n${filter}-- Optional vector ranking before LIMIT:\n-- ORDER BY cosine_distance(c.${quoteIdentifier("embedding")}, ARRAY[...])\n-- Replace ... with ${Number(collection.config.profile.dimensions)} values from this collection's embedding model.\nLIMIT 100;`;
  setEditor(sql);
  switchView("console");
  $("#sql-editor").focus();
}
function graphRevision() {
  if (state.graph.loading) throw new Error("Wait for the current graph to finish loading before making changes.");
  if (!Number.isSafeInteger(state.graph.revision) || state.graph.revision < 0) throw new Error("Refresh the graph before making changes. Its revision must be represented exactly by this browser.");
  return state.graph.revision;
}
function graphError(error, status = "graph-status") {
  if (error.stale) return;
  $("#" + status).textContent = error.code === "stale_revision"
    ? "The collection changed. Your draft is preserved. Refresh the graph, review the changes, then try again. Embedding charges may already have occurred."
    : error.message;
  showError(error);
}
function setGraphBusy(busy) {
  state.graph.busy = busy;
  if (busy) { if (!state.graph.loading) state.graph.generation += 1; state.graph.catalogGeneration += 1; }
  for (const element of $$("#graph-document-form input, #graph-document-form select, #graph-document-form textarea, #graph-document-form button, #graph-relationship-form input, #graph-relationship-form select, #graph-relationship-form button, #graph-create-form input, #graph-create-form select, #graph-create-form button, [data-remove-relationship]")) element.disabled = busy;
  for (const id of ["graph-collection", "graph-refresh", "graph-create-open", "graph-add-open"]) $("#" + id).disabled = busy;
  const builder = fieldBuilders.get("graph-create-fields");
  if (builder) syncFieldBuilder(builder);
  syncGraphDocumentFields(); graphProfileNote();
  updateGraphPaging();
}
function updateGraphPaging() {
  const graph = state.graph;
  $("#graph-previous").disabled = graph.busy || graph.mode !== "pages" || graph.offset <= 0;
  $("#graph-next").disabled = graph.busy || graph.mode !== "pages" || graph.offset + graph.limit >= graph.total;
  $("#graph-back-pages").disabled = graph.busy;
  $$("#graph-neighborhood-controls input, #graph-neighborhood-controls select, #graph-neighborhood-controls button, [data-explore-connections]").forEach((element) => { element.disabled = graph.busy; });
  $$("#graph-relationship-form input, #graph-relationship-form select, #graph-relationship-form button, [data-remove-relationship]").forEach((element) => { element.disabled = graph.busy || graph.loading; });
  $$("#graph-search-form input, #graph-search-form textarea, #graph-search-form select, #graph-search-form button").forEach((element) => { element.disabled = graph.busy || graph.searchBusy; });
  syncGraphFilters();
}

async function loadGraphCollections(preferred = null) {
  const graph = state.graph;
  const generation = ++graph.catalogGeneration;
  try {
    const collections = await request("/v1/graph/collections");
    if (generation !== state.graph.catalogGeneration) throw staleRequest();
    if (!Array.isArray(collections)) throw new Error("The server returned an invalid collection list.");
    graph.collections = collections;
    const wanted = preferred ?? graph.collection;
    const selected = collections.some((item) => item.config.name === wanted) ? wanted : (collections[0]?.config.name || "");
    fillOptions($("#graph-collection"), collections.map((item) => ({ id: item.config.name, label: `${item.config.name} · ${item.document_count} documents` })), selected, "Choose a collection");
    if (selected !== graph.collection) await selectGraphCollection(selected);
    else {
      graphProfileNote();
      renderGraphDocumentFields();
      renderGraphFilters();
      if (selected) await loadGraph();
      else renderGraph();
    }
  } catch (error) { if (generation === state.graph.catalogGeneration) graphError(error); }
}
async function selectGraphCollection(name) {
  if (state.graph.busy) return;
  const graph = state.graph;
  graph.catalogGeneration += 1;
  graph.collection = name;
  graph.generation += 1;
  graph.searchGeneration += 1;
  graph.previewGeneration += 1;
  graph.searchBusy = false; graph.loading = false;
  graph.mode = "pages"; graph.rootChunk = null; graph.rootLabel = ""; graph.pageSelection = null;
  $("#graph-neighborhood-controls").reset();
  graph.offset = 0; graph.revision = null; graph.nodes = []; graph.edges = []; graph.total = 0; graph.selected = null; graph.zoom = 1; graph.pan = { x: 0, y: 0 };
  $("#graph-collection").value = name;
  $("#graph-search-status").textContent = "";
  $("#graph-document-status").textContent = "";
  clear($("#graph-chunk-preview"));
  clear($("#graph-search-results")).append(node("p", "empty-workspace", "Retrieved passages and their source citations will appear here."));
  graphProfileNote(); renderGraphDocumentFields({ reset: true }); renderGraphFilters(); renderGraph(); renderGraphDetails(); updateGraphPaging();
  $("#graph-search-results").setAttribute("aria-busy", "false");
  if (name) await loadGraph();
}
async function loadGraph() {
  const graph = state.graph;
  if (graph.mode === "neighborhood") return loadGraphNeighborhood();
  if (!graph.collection) { $("#graph-status").textContent = "Create or choose a collection first."; return; }
  const generation = ++graph.generation;
  const name = graph.collection;
  const session = state.session;
  graph.loading = true; updateGraphPaging(); updateGraphEmpty();
  $("#graph-status").textContent = graph.notice || "Loading connections…";
  $("#graph-canvas").setAttribute("aria-busy", "true");
  try {
    const result = await request(`${graphPath("/graph")}?limit=100&offset=${graph.offset}`);
    if (session !== state.session || generation !== state.graph.generation || name !== state.graph.collection) throw staleRequest();
    if (!Array.isArray(result.nodes) || !Array.isArray(result.edges)) throw new Error("The server returned an invalid graph.");
    graph.nodes = result.nodes.slice(0, 100);
    graph.edges = result.edges.slice(0, 2000);
    graph.revision = result.revision; graph.total = result.total_nodes;
    graph.offset = result.offset; graph.limit = Math.min(100, result.limit || 100);
    graph.selected = graph.nodes.find((item) => item.chunk_id === graph.selected?.chunk_id) || null;
    $("#graph-status").textContent = `${graph.notice ? `${graph.notice} ` : ""}${result.total_nodes.toLocaleString()} passages · ${result.total_edges.toLocaleString()} directed relationships${result.truncated ? " · showing a bounded page; only links within this page are drawn" : ""}`;
    renderGraph(); renderGraphDetails();
    if (graph.busy) $$("[data-remove-relationship]").forEach((button) => { button.disabled = true; });
    return true;
  } catch (error) { if (session === state.session && generation === state.graph.generation) { graph.revision = null; graphError(error); } return false; }
  finally { if (session === state.session && generation === state.graph.generation) { graph.loading = false; $("#graph-canvas").setAttribute("aria-busy", "false"); updateGraphPaging(); updateGraphEmpty(); } }
}
function graphNeighborhoodOptions() {
  const options = { max_hops: Number($("#graph-neighborhood-hops").value), direction: $("#graph-neighborhood-direction").value, min_weight: Number($("#graph-neighborhood-min-weight").value) };
  if (!Number.isInteger(options.max_hops) || options.max_hops < 0 || options.max_hops > 3
    || !["outgoing", "incoming", "both"].includes(options.direction)
    || !Number.isFinite(options.min_weight) || options.min_weight < 0 || options.min_weight > 1) {
    throw new Error("Choose 0–3 hops, a relationship direction, and a minimum weight between 0 and 1.");
  }
  return options;
}
function renderGraphMode() {
  const graph = state.graph;
  const focused = graph.mode === "neighborhood";
  $("#graph-mode").textContent = focused ? "FOCUSED CONNECTIONS" : "ALL PASSAGES";
  $("#graph-focus-title").textContent = focused ? `Around ${graph.rootLabel}` : "Browse the collection, or explore a passage’s connections.";
  $("#graph-neighborhood-controls").hidden = !focused;
  $("#graph-back-pages").hidden = !focused;
  $("#graph-previous").hidden = focused;
  $("#graph-next").hidden = focused;
  $("#graph-explanation").textContent = focused
    ? "The outlined center is the root passage. Rings and labels mark hop depth; colors identify documents. Arrows retain the original relationship direction. Layout distance does not measure relevance."
    : "Colors identify documents. Lines show cosine similarity, neighboring passages, or your own directed links. Proximity on this map is for readability; it does not measure relevance.";
}
async function exploreGraphConnections(item) {
  const graph = state.graph;
  if (graph.busy || !item || !graph.collection) return;
  if (graph.mode === "pages") graph.pageSelection = graph.selected;
  graph.catalogGeneration += 1;
  graph.mode = "neighborhood"; graph.rootChunk = item.chunk_id; graph.rootLabel = graphLabel(item);
  graph.nodes = []; graph.edges = []; graph.selected = null; graph.revision = null;
  graph.zoom = 1; graph.pan = { x: 0, y: 0 };
  renderGraph(); renderGraphDetails();
  $("#graph-mode").scrollIntoView({ behavior: "smooth", block: "nearest" });
  await loadGraphNeighborhood({ selectRoot: true });
}
async function loadGraphNeighborhood({ selectRoot = false } = {}) {
  const graph = state.graph;
  if (graph.mode !== "neighborhood" || !graph.rootChunk) return false;
  const generation = ++graph.generation;
  const name = graph.collection; const root = graph.rootChunk; const session = state.session;
  graph.loading = true; updateGraphPaging(); updateGraphEmpty();
  $("#graph-canvas").setAttribute("aria-busy", "true");
  $("#graph-status").textContent = graph.notice || "Exploring connections across the collection…";
  try {
    const options = graphNeighborhoodOptions();
    const query = new URLSearchParams({ chunk_id: root, ...options, neighbor_limit: 8, max_nodes: 100, max_edges: 500 });
    const result = await request(`${graphPath("/neighborhood")}?${query}`);
    if (session !== state.session || generation !== state.graph.generation || name !== state.graph.collection || state.graph.mode !== "neighborhood" || root !== state.graph.rootChunk) throw staleRequest();
    if (!Array.isArray(result.nodes) || !Array.isArray(result.edges) || result.root_chunk !== root) throw new Error("The server returned an invalid connection neighborhood.");
    graph.nodes = result.nodes.slice(0, 100); graph.edges = result.edges.slice(0, 500); graph.revision = result.revision;
    const rootNode = graph.nodes.find((item) => item.chunk_id === root);
    if (!rootNode) throw new Error("The connection neighborhood did not include its root passage. Refresh to try again.");
    graph.rootLabel = graphLabel(rootNode);
    graph.selected = (!selectRoot && graph.nodes.find((item) => item.chunk_id === graph.selected?.chunk_id)) || rootNode;
    const bounded = result.truncated || result.nodes.length > 100 || result.edges.length > 500;
    graph.neighborhoodTruncated = bounded;
    $("#graph-status").textContent = `${graph.notice ? `${graph.notice} ` : ""}${graph.nodes.length} passages · ${graph.edges.length} directed relationships · ${options.max_hops} hop${options.max_hops === 1 ? "" : "s"} · ${options.direction === "both" ? "both directions" : options.direction}${bounded ? " · bounded view; more connections may exist" : " · within the selected exploration limits"}`;
    renderGraph(); renderGraphDetails();
    return true;
  } catch (error) {
    if (session === state.session && generation === state.graph.generation) { graph.revision = null; graphError(error); }
    return false;
  } finally {
    if (session === state.session && generation === state.graph.generation) {
      graph.loading = false; $("#graph-canvas").setAttribute("aria-busy", "false"); updateGraphPaging(); updateGraphEmpty();
    }
  }
}
async function returnToGraphPages() {
  const graph = state.graph;
  if (graph.busy) return;
  graph.catalogGeneration += 1; graph.generation += 1;
  graph.mode = "pages"; graph.rootChunk = null; graph.rootLabel = ""; graph.neighborhoodTruncated = false;
  graph.nodes = []; graph.edges = []; graph.revision = null;
  graph.selected = graph.pageSelection;
  graph.zoom = 1; graph.pan = { x: 0, y: 0 };
  renderGraph(); renderGraphDetails();
  await loadGraph();
}
function updateGraphEmpty() {
  const graph = state.graph;
  $("#graph-empty").hidden = graph.nodes.length > 0;
  if (graph.loading) {
    $("#graph-empty h3").textContent = "Loading connections…";
    $("#graph-empty p").textContent = graph.mode === "neighborhood" ? "Following this passage across the collection." : "Opening this page of the collection.";
  } else if (graph.mode === "neighborhood") {
    $("#graph-empty h3").textContent = "Connections are unavailable";
    $("#graph-empty p").textContent = "Check the status above, try another filter, or return to all passages.";
  } else {
    $("#graph-empty h3").textContent = graph.collection ? "Your collection is ready" : "A map of your knowledge";
    $("#graph-empty p").textContent = graph.collection ? "Add a document to create passages and discover connections." : "Create a collection, then add documents to reveal their connections.";
  }
}
function graphDepthLabel(item) {
  return item.chunk_id === state.graph.rootChunk ? "Root passage" : `${item.depth} hop${item.depth === 1 ? "" : "s"} from root`;
}
function renderGraphPointLabels() {
  const graph = state.graph;
  const positions = graphPositions(graph.nodes);
  const byId = new Map(graph.nodes.map((item) => [item.chunk_id, item]));
  for (const point of $$(".graph-point")) {
    const item = byId.get(point.dataset.chunk);
    const label = clear($(".graph-point-label", point));
    const prominent = item.chunk_id === graph.rootChunk || item.chunk_id === graph.selected?.chunk_id;
    const detailed = prominent || (graph.mode === "neighborhood" && graph.nodes.length <= 12);
    if (!detailed && graph.nodes.length > 25) continue;
    const left = positions.get(item.chunk_id).x > 625;
    const x = left ? -15 : 15;
    label.setAttribute("text-anchor", left ? "end" : "start");
    label.setAttribute("x", String(x));
    if (detailed) {
      const title = svgNode("tspan", { x, dy: -4 });
      const name = Array.from(item.title || item.document_id);
      title.textContent = name.length > 23 ? `${name.slice(0, 22).join("")}…` : name.join("");
      const detail = svgNode("tspan", { x, dy: 15, class: "graph-point-caption" });
      detail.textContent = `${Number.isInteger(item.ordinal) ? `Passage ${item.ordinal + 1}` : "Passage"}${graph.mode === "neighborhood" ? ` · ${item.chunk_id === graph.rootChunk ? "root" : `${item.depth} hop${item.depth === 1 ? "" : "s"}`}` : ""}`;
      label.append(title, detail);
    } else label.textContent = String((item.ordinal ?? 0) + 1);
  }
}

function svgNode(tag, attributes = {}) {
  const element = document.createElementNS("http://www.w3.org/2000/svg", tag);
  for (const [key, value] of Object.entries(attributes)) element.setAttribute(key, String(value));
  return element;
}
function graphPositions(nodes) {
  if (state.graph.mode === "neighborhood") {
    const positions = new Map([[state.graph.rootChunk, { x: 400, y: 230 }]]);
    const depths = [...new Set(nodes.filter((item) => item.chunk_id !== state.graph.rootChunk).map((item) => item.depth))].sort((a, b) => a - b);
    const maxDepth = Math.max(1, ...depths);
    for (const depth of depths) {
      const members = nodes.filter((item) => item.depth === depth && item.chunk_id !== state.graph.rootChunk);
      const radiusX = 310 * depth / maxDepth; const radiusY = 180 * depth / maxDepth;
      members.forEach((item, index) => {
        const angle = index / members.length * Math.PI * 2 - Math.PI / 2 + (depth - 1) * .25;
        positions.set(item.chunk_id, { x: 400 + Math.cos(angle) * radiusX, y: 230 + Math.sin(angle) * radiusY });
      });
    }
    return positions;
  }
  const documents = [...new Set(nodes.map((item) => item.document_id))];
  const positions = new Map();
  documents.forEach((documentId, documentIndex) => {
    const angle = (documentIndex / documents.length) * Math.PI * 2 - Math.PI / 2;
    const centerX = documents.length === 1 ? 400 : 400 + Math.cos(angle) * 245;
    const centerY = documents.length === 1 ? 230 : 230 + Math.sin(angle) * 133;
    const members = nodes.filter((item) => item.document_id === documentId);
    members.forEach((item, index) => {
      const theta = index * 2.399963;
      const radius = members.length === 1 ? 0 : Math.sqrt(index + .5) * Math.min(17, 82 / Math.sqrt(members.length));
      positions.set(item.chunk_id, { x: centerX + Math.cos(theta) * radius, y: centerY + Math.sin(theta) * radius });
    });
  });
  return positions;
}
function renderGraph() {
  const graph = state.graph;
  const svg = clear($("#graph-canvas"));
  const defs = svgNode("defs");
  const marker = svgNode("marker", { id: "graph-arrow", viewBox: "0 0 10 10", refX: 20, refY: 5, markerWidth: 5, markerHeight: 5, orient: "auto-start-reverse" });
  marker.append(svgNode("path", { d: "M 0 0 L 10 5 L 0 10 z", fill: "#ff9e64" })); defs.append(marker); svg.append(defs);
  const layer = svgNode("g", { id: "graph-drawing" }); svg.append(layer);
  const positions = graphPositions(graph.nodes);
  if (graph.mode === "neighborhood" && graph.nodes.length) {
    const depths = [...new Set(graph.nodes.map((item) => item.depth).filter((depth) => depth > 0))].sort((a, b) => a - b);
    const maxDepth = Math.max(1, ...depths);
    for (const depth of depths) layer.append(svgNode("ellipse", { cx: 400, cy: 230, rx: 310 * depth / maxDepth, ry: 180 * depth / maxDepth, class: "graph-depth-ring", "aria-hidden": "true" }));
  }
  for (const edge of graph.edges) {
    const from = positions.get(edge.from_chunk); const to = positions.get(edge.to_chunk);
    if (!from || !to) continue;
    const kind = ["semantic", "adjacent"].includes(edge.kind) ? edge.kind : "custom";
    const line = svgNode("line", { x1: from.x, y1: from.y, x2: to.x, y2: to.y, class: `graph-edge ${kind}`, "data-kind": kind, "data-from": edge.from_chunk, "data-to": edge.to_chunk });
    if (kind === "custom" || graph.mode === "neighborhood") line.setAttribute("marker-end", "url(#graph-arrow)");
    const title = svgNode("title"); title.textContent = `${edge.kind} · weight ${formatGraphScore(edge.weight)}`; line.append(title); layer.append(line);
  }
  const list = clear($("#graph-node-list"));
  graph.nodes.forEach((item, index) => {
    const position = positions.get(item.chunk_id);
    const label = graphLabel(item);
    const depthLabel = graph.mode === "neighborhood" ? ` · ${graphDepthLabel(item)}` : "";
    const point = svgNode("g", { transform: `translate(${position.x} ${position.y})`, class: "graph-point", role: "button", tabindex: 0, "aria-label": `View ${label}${depthLabel}`, "aria-pressed": "false", "data-chunk": item.chunk_id });
    point.style.setProperty("--node-color", graphColor(item.document_id));
    point.classList.toggle("graph-root", item.chunk_id === graph.rootChunk);
    if (graph.mode === "neighborhood") point.dataset.depth = item.depth;
    if (item.chunk_id === graph.rootChunk) point.append(svgNode("circle", { r: 16, class: "graph-root-outline" }));
    point.append(svgNode("circle", { r: 18, class: "graph-hit-target" }), svgNode("circle", { r: item.chunk_id === graph.rootChunk ? 10 : 7.5 }));
    const title = svgNode("title"); title.textContent = label + depthLabel; point.append(title);
    point.append(svgNode("text", { x: 15, y: 4, class: "graph-point-label" }));
    point.addEventListener("click", () => { if (!graph.dragged) selectGraphNode(item); });
    point.addEventListener("keydown", (event) => {
      if (["Enter", " "].includes(event.key)) { event.preventDefault(); selectGraphNode(item); }
      if (["ArrowRight", "ArrowDown", "ArrowLeft", "ArrowUp"].includes(event.key)) {
        event.preventDefault();
        const step = ["ArrowRight", "ArrowDown"].includes(event.key) ? 1 : -1;
        $$(".graph-point")[((index + step) + graph.nodes.length) % graph.nodes.length]?.focus();
      }
    });
    layer.append(point);
    const button = node("button", "graph-list-node"); button.type = "button"; button.dataset.chunk = item.chunk_id;
    button.setAttribute("aria-label", `Read ${label}${depthLabel}`); button.setAttribute("aria-pressed", "false");
    button.style.setProperty("--node-color", graphColor(item.document_id)); button.append(node("i"), node("span", "", label));
    if (depthLabel) button.append(node("small", "graph-depth-badge", item.chunk_id === graph.rootChunk ? "Root" : `${item.depth} hop${item.depth === 1 ? "" : "s"}`));
    button.addEventListener("click", () => selectGraphNode(item)); list.append(button);
  });
  const legend = clear($("#graph-legend"));
  const documents = new Map(graph.nodes.map((item) => [item.document_id, item.title || item.document_id]));
  for (const [id, title] of documents) { const entry = node("span", "", ""); entry.style.setProperty("--node-color", graphColor(id)); entry.append(node("i"), document.createTextNode(title)); legend.append(entry); }
  updateGraphEmpty();
  $("#graph-range").textContent = graph.mode === "neighborhood" ? `${graph.nodes.length} passages in focused view${graph.neighborhoodTruncated ? " · bounded" : ""}` : graph.total ? `${graph.offset + 1}–${graph.offset + graph.nodes.length} of ${graph.total.toLocaleString()} passages` : "0 passages";
  $("#graph-list-count").textContent = `(${graph.nodes.length})`;
  renderGraphMode(); applyGraphFilters(); applyGraphZoom(); highlightGraphSelection(); updateGraphPaging();
}
function applyGraphFilters() {
  const enabled = new Set($$("[data-graph-kind]").filter((input) => input.checked).map((input) => input.dataset.graphKind));
  $$(".graph-edge").forEach((edge) => { edge.style.display = enabled.has(edge.dataset.kind) ? "" : "none"; });
}
function applyGraphZoom() {
  const zoom = state.graph.zoom;
  const focus = zoom > 1 && state.graph.selected ? graphPositions(state.graph.nodes).get(state.graph.selected.chunk_id) : null;
  const center = focus || { x: 400, y: 230 };
  $("#graph-drawing")?.setAttribute("transform", `translate(${400 - center.x * zoom + state.graph.pan.x} ${230 - center.y * zoom + state.graph.pan.y}) scale(${zoom})`);
  $("#graph-zoom-reset").textContent = `${Number(zoom.toFixed(1))}×`;
}
function highlightGraphSelection() {
  const id = state.graph.selected?.chunk_id;
  $$("[data-chunk]").forEach((element) => element.setAttribute("aria-pressed", String(element.dataset.chunk === id)));
  $$(".graph-edge").forEach((edge) => edge.classList.toggle("connected", edge.dataset.from === id || edge.dataset.to === id));
  renderGraphPointLabels();
}
function selectGraphNode(item) {
  if (state.graph.busy) return;
  state.graph.selected = item; highlightGraphSelection(); renderGraphDetails(); updateGraphPaging();
}
function graphCitation(item) {
  const citation = node("div", "graph-citation");
  const source = String(item.source || item.document_id);
  let url;
  try { url = new URL(source); } catch { /* A filename is also a valid citation. */ }
  if (url && ["http:", "https:"].includes(url.protocol)) {
    const link = node("a", "", source); link.href = url.href; link.target = "_blank"; link.rel = "noopener noreferrer"; citation.append(link);
  } else citation.append(document.createTextNode(source));
  citation.append(node("div", "", `UTF-8 bytes ${item.start_byte}–${item.end_byte} · ${item.document_id}`));
  return citation;
}
function formatGraphScore(value) { return typeof value === "number" && Number.isFinite(value) ? value.toFixed(4) : "—"; }
function renderGraphDetails() {
  const target = clear($("#graph-details")); const selected = state.graph.selected;
  target.append(node("span", "panel-kicker", "PASSAGE DETAILS"));
  $("#graph-relationship-form").hidden = !selected;
  if (!selected) { target.append(node("h3", "", "Follow a connection"), node("p", "", "Select a point or a passage from the list to read its source and relationships.")); return; }
  target.append(node("h3", "", selected.title || selected.document_id), graphCitation(selected), node("p", "graph-passage", selected.text), node("code", "graph-chunk-id", selected.chunk_id));
  if (state.graph.mode === "neighborhood" && Number.isInteger(selected.depth)) target.append(node("p", "graph-depth-description", graphDepthLabel(selected)));
  const explore = node("button", "button ghost compact graph-explore-button", "Explore connections"); explore.type = "button"; explore.dataset.exploreConnections = "";
  explore.disabled = state.graph.busy;
  explore.addEventListener("click", () => void exploreGraphConnections(selected)); target.append(explore);
  if (selected.metadata && Object.keys(selected.metadata).length) {
    const metadata = node("details", "form-advanced"); metadata.append(node("summary", "", "Source metadata"));
    metadata.addEventListener("toggle", () => { if (metadata.open && metadata.childElementCount === 1) metadata.append(node("p", "graph-passage", JSON.stringify(selected.metadata, null, 2))); }); target.append(metadata);
  }
  const relationships = node("div", "graph-detail-links");
  const edges = state.graph.edges.filter((edge) => edge.from_chunk === selected.chunk_id || edge.to_chunk === selected.chunk_id);
  const displayed = new Set();
  for (const edge of edges) {
    const outgoing = edge.from_chunk === selected.chunk_id;
    const otherId = outgoing ? edge.to_chunk : edge.from_chunk;
    const key = `${edge.kind}:${otherId}:${["semantic", "adjacent"].includes(edge.kind) ? "both" : outgoing}`;
    if (displayed.has(key)) continue; displayed.add(key);
    const other = state.graph.nodes.find((item) => item.chunk_id === otherId);
    const row = node("div", "graph-detail-link"); const link = node("button", "", `${outgoing ? "→" : "←"} ${other ? graphLabel(other) : otherId}`); link.type = "button"; link.disabled = !other;
    link.addEventListener("click", () => selectGraphNode(other));
    row.append(link, node("small", "", `${edge.kind} · weight ${formatGraphScore(edge.weight)}`));
    if (!["semantic", "adjacent"].includes(edge.kind)) {
      const remove = node("button", "button danger ghost compact", "Remove relationship"); remove.type = "button"; remove.dataset.removeRelationship = "";
      remove.setAttribute("aria-label", `Remove ${edge.kind} relationship to ${otherId}`);
      remove.addEventListener("click", () => void mutateGraphRelationship("DELETE", edge)); row.append(remove);
    }
    relationships.append(row);
  }
  if (!edges.length) relationships.append(node("p", "field-hint", state.graph.mode === "neighborhood" ? "No relationships match this focused view. Try different exploration filters." : "No relationships to other passages on this page. Explore connections to look beyond it."));
  target.append(relationships);
  fillOptions($("#graph-link-target"), state.graph.nodes.filter((item) => item.chunk_id !== selected.chunk_id).map((item) => ({ id: item.chunk_id, label: graphLabel(item) })), null, "Choose a target passage");
}
async function graphMutation(action, statusId = "graph-status") {
  if (state.graph.busy) return;
  const session = state.session;
  setGraphBusy(true);
  try { await action(); }
  catch (error) { graphError(error, statusId); }
  finally { if (session === state.session) setGraphBusy(false); }
}
async function mutateGraphRelationship(method, edge = null) {
  return graphMutation(async () => {
    if (!state.graph.selected) throw new Error("Select a passage first.");
    const payload = edge ? { from_chunk: edge.from_chunk, to_chunk: edge.to_chunk, kind: edge.kind }
      : { from_chunk: state.graph.selected.chunk_id, to_chunk: $("#graph-link-target").value, kind: $("#graph-link-kind").value.trim(), weight: Number($("#graph-link-weight").value) };
    if (!payload.to_chunk || payload.to_chunk === payload.from_chunk) throw new Error("Choose a different target passage.");
    if (!/^[a-z][a-z0-9_]{0,63}$/.test(payload.kind) || ["adjacent", "semantic"].includes(payload.kind)) throw new Error("Use a custom label with lowercase letters, digits, or underscores; adjacent and semantic are reserved.");
    if (method === "POST" && (!Number.isFinite(payload.weight) || payload.weight < 0 || payload.weight > 1)) throw new Error("Relationship weight must be between 0 and 1.");
    payload.expected_revision = graphRevision();
    $("#graph-status").textContent = method === "POST" ? "Saving relationship…" : "Removing relationship…";
    await request(graphPath("/relationships"), { method, body: JSON.stringify(payload) });
    const refreshed = await loadGraph();
    $("#graph-status").textContent = (method === "POST" ? "Directed relationship saved." : "Directed relationship removed.") + (refreshed ? "" : " Refresh failed; refresh the graph before making further changes.");
  });
}
async function createGraphCollection(event) {
  event.preventDefault();
  return graphMutation(async () => {
    const name = $("#graph-create-name").value.trim();
    if (!/^[a-z][a-z0-9_]{0,47}$/.test(name)) throw new Error("Use a lowercase collection name with letters, digits, and underscores.");
    const document_columns = readFieldBuilder("graph-create-fields");
    const semantic_neighbors = Number($("#graph-create-neighbors").value);
    const semantic_threshold = Number($("#graph-create-threshold").value);
    if (!$("#graph-create-neighbors").value.trim() || !$("#graph-create-threshold").value.trim() || !Number.isInteger(semantic_neighbors) || semantic_neighbors < 0 || semantic_neighbors > 16 || !Number.isFinite(semantic_threshold) || semantic_threshold < 0 || semantic_threshold > 1) throw new Error("Automatic links need 0–16 neighbors and a cosine threshold between 0 and 1.");
    const session = state.session;
    const result = await request("/v1/graph/collections", { method: "POST", body: JSON.stringify({ name, semantic_neighbors, semantic_threshold, document_columns }) });
    $("#graph-create-dialog").close();
    state.graph.collections = [...state.graph.collections.filter((item) => item.config.name !== result.config.name), result];
    state.graph.collection = result.config.name;
    state.graph.revision = result.revision;
    state.graph.mode = "pages"; state.graph.rootChunk = null; state.graph.rootLabel = ""; state.graph.pageSelection = null;
    state.graph.nodes = []; state.graph.edges = []; state.graph.selected = null; state.graph.offset = 0;
    renderGraphDocumentFields({ reset: true });
    switchView("connections");
    await loadGraphCollections(result.config.name);
    if (session !== state.session) throw staleRequest();
    await loadTables({ quiet: true, force: true });
    if (session !== state.session) throw staleRequest();
    $("#graph-status").textContent = "Collection created. Add your first document to connect its passages.";
    $("#graph-document-panel").open = true;
    $("#graph-document-id").focus();
  }, "graph-create-status");
}
function graphDocumentInput() {
  const text = $("#graph-document-text").value;
  if (!text.trim()) throw new Error("Paste document text first.");
  if (new TextEncoder().encode(text).length > 1048576) throw new Error("Document text exceeds 1 MiB. Split it into smaller documents.");
  const chunking = { max_characters: Number($("#graph-chunk-size").value), overlap_characters: Number($("#graph-chunk-overlap").value), max_chunks: Number($("#graph-chunk-count").value) };
  if (!Number.isInteger(chunking.max_characters) || chunking.max_characters < 1 || chunking.max_characters > 8000 || !Number.isInteger(chunking.overlap_characters) || chunking.overlap_characters < 0 || chunking.overlap_characters >= chunking.max_characters || !Number.isInteger(chunking.max_chunks) || chunking.max_chunks < 1 || chunking.max_chunks > 256) throw new Error("Use 1–8,000 characters per chunk, a smaller nonnegative overlap, and 1–256 chunks.");
  return { text, title: $("#graph-document-title").value, chunking };
}
async function previewGraphDocument() {
  if (state.graph.busy) return;
  const generation = ++state.graph.previewGeneration;
  try {
    const payload = graphDocumentInput();
    $("#graph-document-status").textContent = "Preparing a preview without provider requests…";
    const result = await request("/v1/graph/chunk", { method: "POST", body: JSON.stringify(payload) });
    if (generation !== state.graph.previewGeneration) throw staleRequest();
    const target = clear($("#graph-chunk-preview"));
    for (const chunk of result.chunks.slice(0, 256)) {
      const details = node("details"); details.append(node("summary", "", `Passage ${chunk.ordinal + 1} · bytes ${chunk.byte_start}–${chunk.byte_end}${chunk.heading ? ` · ${chunk.heading}` : ""}`));
      details.addEventListener("toggle", () => { if (details.open && details.childElementCount === 1) details.append(node("pre", "", chunk.embedding_text)); }); target.append(details);
    }
    $("#graph-document-status").textContent = `${result.chunks.length} chunks · ${result.embedding_bytes.toLocaleString()} embedding bytes. Review the title and heading context by opening a passage.`;
  } catch (error) { if (generation === state.graph.previewGeneration) graphError(error, "graph-document-status"); }
}
async function requireGraphProfile() {
  const profile = graphCollection()?.config.profile;
  if (!profile) throw new Error("Choose a collection first.");
  const settings = state.embeddings || await loadEmbeddingSettings();
  if (!settings.configured) throw new Error("Add an embedding API key in Settings first.");
  if (["provider", "model", "dimensions"].some((key) => profile[key] !== settings[key])) throw new Error(`This collection requires ${embeddingDescription(profile)}. Restore that profile in Settings before sending text to a provider.`);
  return settings;
}
async function saveGraphDocument(event) {
  event.preventDefault();
  return graphMutation(async () => {
    const name = state.graph.collection;
    const session = state.session;
    if (!graphCollection()) throw new Error("Choose a collection first.");
    const payload = { ...graphDocumentInput(), id: $("#graph-document-id").value.trim(), source: $("#graph-document-source").value, metadata: readGraphDocumentFields(), expected_revision: graphRevision() };
    if (!payload.id) throw new Error("Enter a stable document ID.");
    // The server decides whether text changed. Metadata-only saves and intact
    // replays need no provider key, so do not block them with query preflight.
    const config = state.embeddings || await loadEmbeddingSettings().catch((error) => { if (error.stale) throw error; return null; });
    if (session !== state.session || name !== state.graph.collection) throw staleRequest();
    $("#graph-document-status").textContent = "Saving document… New or changed text will be embedded; unchanged text reuses its existing vectors.";
    const result = await request(graphPath("/documents"), { method: "POST", timeout: ((config?.timeout_seconds || 120) + 15) * 1000, body: JSON.stringify(payload) });
    await loadGraph();
    if (session !== state.session || name !== state.graph.collection) throw staleRequest();
    $("#graph-document-status").textContent = result.unchanged ? "Document unchanged. Its existing embeddings were reused."
      : result.embeddings_reused ? "Fields updated. Existing embeddings and relationships reused."
      : `${result.replaced ? "Replaced" : "Saved"} document · ${result.chunks} chunks · ${result.edges_created} relationships · ${result.embedding_usage?.total_tokens ?? 0} embedding tokens.`;
    void loadTables({ quiet: true });
  }, "graph-document-status");
}
function suggestedGraphSeeds(candidates) {
  return Math.min(12, Math.max(1, candidates - Math.max(1, Math.floor(candidates / 4))));
}
function updateGraphSeedBudget() {
  const candidates = Number($("#graph-candidates").value);
  const valid = Number.isInteger(candidates) && candidates >= 1 && candidates <= 100;
  if (!state.graph.seedExplicit && valid) $("#graph-seeds").value = suggestedGraphSeeds(candidates);
  const seeds = Number($("#graph-seeds").value);
  let hint = state.graph.seedExplicit
    ? "Your seed choice is preserved when the candidate budget changes."
    : "Suggested seeds adjust with the candidate budget to leave room for connected passages.";
  if (valid && seeds > candidates) hint += " Lower seeds to fit within the candidate budget before searching.";
  else if (valid && seeds === candidates && Number($("#graph-hops").value) > 0) hint += " Every candidate slot is a seed; graph expansion cannot add another passage.";
  $("#graph-seed-hint").textContent = hint;
}
function graphRetrievalOptions() {
  const numeric = (selector) => $(selector).value.trim() ? Number($(selector).value) : NaN;
  const payload = {
    candidate_limit: numeric("#graph-candidates"), seed_limit: numeric("#graph-seeds"),
    max_results: numeric("#graph-result-limit"), max_hops: numeric("#graph-hops"),
    neighbor_limit: numeric("#graph-neighbors"), diversity: numeric("#graph-diversity"),
    max_context_bytes: numeric("#graph-context-budget"), max_per_document: numeric("#graph-per-document"),
    direction: $("#graph-retrieval-direction").value, min_weight: numeric("#graph-retrieval-min-weight"),
    vector_weight: 1, lexical_weight: 1,
  };
  for (const [key, label, min, max] of [["candidate_limit", "Candidate passages", 1, 100], ["seed_limit", "Starting passages", 1, 20], ["max_results", "Maximum results", 1, 100], ["max_hops", "Connection depth", 0, 3], ["neighbor_limit", "Neighbors per passage", 1, 32], ["max_context_bytes", "Context budget", 1, 1048576], ["max_per_document", "Results per document", 1, 100]]) {
    if (!Number.isInteger(payload[key]) || payload[key] < min || payload[key] > max) throw new Error(`${label} must be a whole number between ${min} and ${max}.`);
  }
  const seedsPerDocument = $("#graph-seeds-per-document");
  if (seedsPerDocument.value.trim() || seedsPerDocument.validity.badInput) {
    const limit = numeric("#graph-seeds-per-document");
    if (!Number.isInteger(limit) || limit < 1 || limit > 20) throw new Error("Starting passages per document must be a whole number between 1 and 20, or blank for no cap.");
    payload.max_seeds_per_document = limit;
  }
  if (payload.max_results > payload.candidate_limit) throw new Error("Maximum results must not exceed the candidate passage count.");
  if (payload.seed_limit > payload.candidate_limit) throw new Error("Starting passages must not exceed the candidate passage count. Lower seeds or use the suggested value.");
  if (!Number.isFinite(payload.diversity) || payload.diversity < 0 || payload.diversity > 1) throw new Error("Diversity must be between 0 and 1.");
  if (!Number.isFinite(payload.min_weight) || payload.min_weight < 0 || payload.min_weight > 1) throw new Error("Minimum relationship weight must be between 0 and 1.");
  if (!["outgoing", "incoming", "both"].includes(payload.direction)) throw new Error("Choose a valid relationship direction.");
  const kind = $("#graph-retrieval-kind").value.trim();
  if (kind && !validGraphRelationshipKind(kind)) throw new Error("Relationship type must start with a lowercase letter and use only lowercase letters, digits, or underscores, up to 64 characters.");
  if (kind) payload.kind = kind;
  return payload;
}
function validGraphRelationshipKind(kind) {
  return typeof kind === "string" && kind === kind.trim() && /^[a-z][a-z0-9_]{0,63}$/.test(kind);
}
function validatedRetrievalPath(hit) {
  const path = hit.retrieval_path;
  const validId = (value) => typeof value === "string" && value.trim().length > 0 && !value.includes("\0") && new TextEncoder().encode(value).length <= 1024;
  if (!path || !validId(path.seed_chunk_id) || !validId(hit.chunk_id) || !Array.isArray(path.edges) || path.edges.length < 1 || path.edges.length > 3) return null;
  let current = path.seed_chunk_id;
  const steps = [];
  for (const edge of path.edges) {
    if (!edge || !validId(edge.from_chunk) || !validId(edge.to_chunk) || edge.from_chunk === edge.to_chunk
      || !validGraphRelationshipKind(edge.kind) || typeof edge.weight !== "number" || !Number.isFinite(edge.weight) || edge.weight < 0 || edge.weight > 1) return null;
    const outgoing = edge.from_chunk === current;
    if (!outgoing && edge.to_chunk !== current) return null;
    const next = outgoing ? edge.to_chunk : edge.from_chunk;
    steps.push({ edge, current, next, outgoing }); current = next;
  }
  return current === hit.chunk_id ? { seed: path.seed_chunk_id, steps } : null;
}
function renderGraphRetrievalPath(hit, returnedPassages) {
  if (hit.retrieval_path === undefined || hit.retrieval_path === null) return null;
  const details = node("details", "graph-retrieval-path");
  details.append(node("summary", "", "How this passage was found"));
  const path = validatedRetrievalPath(hit);
  details.addEventListener("toggle", () => {
    if (!details.open || details.childElementCount > 1) return;
    if (!path) { details.append(node("p", "field-hint", "The server returned an incomplete connection explanation. The passage is still available, but this path cannot be shown reliably.")); return; }
    details.append(node("p", "graph-path-intro", "Connections are listed from the starting seed to this result. Arrows show each stored relationship’s direction, including links followed in reverse."));
    const seed = node("div", "graph-path-seed"); seed.append(node("strong", "", "Starting seed"), node("code", "", path.seed));
    const seedPassage = returnedPassages.get(path.seed);
    seed.append(node("small", "", seedPassage ? graphLabel(seedPassage) : "Identifier only; this source passage was not returned.")); details.append(seed);
    const list = node("ol", "graph-path-steps");
    for (const [index, step] of path.steps.entries()) {
      const item = node("li"); const heading = node("div", "graph-path-edge-label");
      heading.append(node("strong", "", `${index + 1}. ${step.edge.kind}`), node("span", "", `weight ${formatGraphScore(step.edge.weight)} · followed ${step.outgoing ? "outgoing" : "incoming"}`));
      const connection = node("div", "graph-path-connection");
      const identity = (id) => {
        const target = node("div", "graph-path-node"); target.append(node("code", "", id));
        target.append(node("small", "", returnedPassages.has(id) ? graphLabel(returnedPassages.get(id)) : "Connection identifier · source passage not returned"));
        return target;
      };
      const arrow = node("span", "graph-path-arrow", step.outgoing ? "→" : "←"); arrow.setAttribute("role", "img"); arrow.setAttribute("aria-label", step.outgoing ? "Stored relationship points right" : "Stored relationship points left");
      connection.append(identity(step.current), arrow, identity(step.next)); item.append(heading, connection); list.append(item);
    }
    details.append(list);
    const hiddenIds = [...new Set([path.seed, ...path.steps.map((step) => step.next)])].filter((id) => id !== hit.chunk_id && !returnedPassages.has(id));
    if (hiddenIds.length) {
      const actions = node("div", "graph-path-actions");
      for (const id of hiddenIds) {
        const button = node("button", "button ghost compact", id === path.seed ? "Explore starting seed" : "Explore bridge connection");
        button.type = "button"; button.dataset.exploreConnections = ""; button.disabled = state.graph.busy;
        button.setAttribute("aria-label", `Explore connections for ${id}`);
        button.addEventListener("click", () => void exploreGraphConnections({ chunk_id: id })); actions.append(button);
      }
      details.append(actions);
    }
  });
  return details;
}

async function retrieveGraphContext(event) {
  event.preventDefault();
  const graph = state.graph;
  if (graph.searchBusy || graph.busy) return;
  const session = state.session;
  const collection = graph.collection;
  const generation = ++graph.searchGeneration;
  const current = () => session === state.session && graph === state.graph && collection === state.graph.collection && generation === state.graph.searchGeneration;
  graph.searchBusy = true; updateGraphPaging();
  $("#graph-search-results").setAttribute("aria-busy", "true");
  try {
    const text = $("#graph-question").value.trim();
    if (!text) throw new Error("Enter a question first.");
    const maxBytes = $("#graph-reranker").value === "voyage" ? 7872 : 8191;
    if (text.includes("\0") || new TextEncoder().encode(text).length > maxBytes) throw new Error(`Questions must not contain NUL characters and must fit within ${maxBytes} UTF-8 bytes.`);
    const options = graphRetrievalOptions();
    const documentFilters = readGraphFilters();
    const config = await requireGraphProfile();
    if (!current()) throw staleRequest();
    const reranker = $("#graph-reranker").value;
    if (reranker === "voyage") {
      const settings = state.reranking || await loadRerankingSettings();
      if (!settings?.configured) throw new Error("Add a Voyage reranking key in Settings first.");
    }
    if (!current()) throw staleRequest();
    const payload = { text, ...options, reranker };
    if (documentFilters.length) payload.document_filters = documentFilters;
    $("#graph-search-status").textContent = reranker === "voyage" ? "Retrieving candidates, then asking Voyage to rerank their context…" : "Combining vector matches, lexical evidence, and connected passages…";
    const result = await request(graphPath("/retrieve"), { method: "POST", timeout: (config.timeout_seconds + (reranker === "voyage" ? state.reranking.timeout_seconds : 0) + 15) * 1000, body: JSON.stringify(payload) });
    if (!current()) throw staleRequest();
    renderGraphContext(result, documentFilters);
    $("#graph-search-status").textContent = `${result.hits.length} passage${result.hits.length === 1 ? "" : "s"} retrieved. Scores rank candidates; they are not confidence or factual certainty.`;
  } catch (error) { if (current()) graphError(error, "graph-search-status"); }
  finally { if (current()) { graph.searchBusy = false; updateGraphPaging(); $("#graph-search-results").setAttribute("aria-busy", "false"); } }
}
function renderGraphContext(result, documentFilters = []) {
  const target = clear($("#graph-search-results"));
  const method = result.reranking.method === "voyage" ? `Voyage ${result.reranking.model}` : "Local hybrid ranking";
  target.append(node("p", "graph-context-summary", `${method} · ${result.candidate_count} candidates · ${result.context_bytes.toLocaleString()} context bytes${result.truncated ? " · bounded results" : ""}`));
  if (documentFilters.length) target.append(node("p", "graph-context-summary graph-applied-filters", `Document scope · ${describeGraphFilters(documentFilters)}. Unmatched documents are excluded from this context.`));
  if (!result.hits.length) { target.append(node("p", "empty-workspace", documentFilters.length ? "No passages fit these document filters, question, and context budget. Review the filters or widen the scope." : "No passages fit this query and context budget.")); return; }
  const returnedPassages = new Map(result.hits.slice(0, 100).map((hit) => [hit.chunk_id, hit]));
  for (const [index, hit] of result.hits.slice(0, 100).entries()) {
    const card = node("article", "graph-hit"); card.append(node("h4", "", `${index + 1}. ${hit.title || hit.document_id}`), graphCitation(hit), node("p", "graph-passage", hit.text));
    const scores = node("div", "graph-scores");
    for (const [label, score] of [["Cosine", hit.similarity], ["Lexical", hit.lexical_score], ["Fusion", hit.fusion_score], ["Rerank", hit.rerank_score], ["Selection", hit.selection_score]]) if (score !== null && score !== undefined) scores.append(node("span", "", `${label} ${formatGraphScore(score)}`));
    card.append(scores, node("p", "field-hint", `${hit.seed ? "Seed passage" : `Connected passage · ${hit.depth} hop${hit.depth === 1 ? "" : "s"}`}`));
    const explanation = renderGraphRetrievalPath(hit, returnedPassages);
    if (explanation) card.append(explanation);
    const inspect = node("button", "button ghost compact", "Inspect passage"); inspect.type = "button";
    inspect.addEventListener("click", () => { selectGraphNode(state.graph.nodes.find((item) => item.chunk_id === hit.chunk_id) || hit); $("#graph-details").scrollIntoView({ behavior: "smooth", block: "nearest" }); });
    const explore = node("button", "button ghost compact", "Explore connections"); explore.type = "button"; explore.dataset.exploreConnections = ""; explore.disabled = state.graph.busy;
    explore.addEventListener("click", () => void exploreGraphConnections(hit));
    const actions = node("div", "inline-actions"); actions.append(inspect, explore); card.append(actions); target.append(card);
  }
}
async function loadRerankingSettings() {
  const generation = ++state.rerankingGeneration;
  try {
    const settings = await request("/v1/settings/reranking");
    if (generation !== state.rerankingGeneration) throw staleRequest();
    state.reranking = settings;
    fillOptions($("#rerank-model"), settings.models, settings.model);
    $("#rerank-timeout").value = settings.timeout_seconds;
    $("#rerank-concurrency").value = settings.max_concurrent_requests;
    $("#rerank-key-status").textContent = settings.configured ? "Voyage has an active reranking key. Leave the field blank to keep it." : "Add a reranking key or configure VOYAGE_API_KEY on the server.";
    $("#rerank-settings-status").textContent = settings.persistence === "durable" ? "Model and request settings persist. The API key stays in server memory." : "These settings last for this server session.";
    return settings;
  } catch (error) {
    if (!error.stale && generation === state.rerankingGeneration) { $("#rerank-settings-status").textContent = error.message; if (error.status === 401) showError(error); }
    throw error;
  }
}
async function saveRerankingSettings(event) {
  event.preventDefault();
  if (state.rerankingBusy) return;
  const session = state.session;
  const generation = ++state.rerankingGeneration;
  state.rerankingBusy = true; $("#rerank-save").disabled = true;
  try {
    const key = $("#rerank-api-key").value.trim();
    if (key && $("#rerank-clear-key").checked) throw new Error("Enter a replacement key or clear the current key, not both.");
    const payload = { model: $("#rerank-model").value, timeout_seconds: Number($("#rerank-timeout").value), max_concurrent_requests: Number($("#rerank-concurrency").value), clear_api_key: $("#rerank-clear-key").checked };
    if (key) payload.api_key = key;
    $("#rerank-api-key").value = "";
    $("#rerank-settings-status").textContent = "Saving…";
    const settings = await request("/v1/settings/reranking", { method: "PUT", body: JSON.stringify(payload) });
    if (generation !== state.rerankingGeneration) throw staleRequest();
    state.reranking = settings; $("#rerank-clear-key").checked = false;
    $("#rerank-key-status").textContent = settings.configured ? "Voyage has an active reranking key." : "No active reranking key.";
    $("#rerank-settings-status").textContent = "Reranking settings saved. Keys stay in server memory.";
  } catch (error) { if (!error.stale && generation === state.rerankingGeneration) { $("#rerank-settings-status").textContent = error.message; showError(error); } }
  finally { if (session === state.session) { state.rerankingBusy = false; $("#rerank-save").disabled = false; } }
}
function bindGraphEvents() {
  $("#graph-back-pages").addEventListener("click", () => void returnToGraphPages());
  $("#graph-neighborhood-controls").addEventListener("submit", (event) => { event.preventDefault(); if (!state.graph.busy) void loadGraphNeighborhood(); });
  $("#graph-neighborhood-controls").addEventListener("input", () => {
    if (state.graph.mode !== "neighborhood") return;
    state.graph.generation += 1; state.graph.loading = false;
    $("#graph-canvas").setAttribute("aria-busy", "false"); updateGraphPaging(); updateGraphEmpty();
    $("#graph-status").textContent = "Exploration filters changed. Apply them to update the focused view.";
  });
  $("#graph-collection").addEventListener("change", (event) => void selectGraphCollection(event.target.value));
  $("#graph-refresh").addEventListener("click", () => { if (!state.graph.busy) { state.graph.notice = ""; void loadGraphCollections(); } });
  $("#graph-create-open").addEventListener("click", openCreateCollection);
  $("#graph-create-settings").addEventListener("click", () => { $("#graph-create-dialog").close(); switchView("settings"); });
  $("#graph-view-data").addEventListener("click", () => void viewGraphData());
  $("#graph-open-sql").addEventListener("click", () => { try { openGraphSql(); } catch (error) { graphError(error); } });
  $("#graph-create-form").addEventListener("submit", createGraphCollection);
  $("#graph-add-open").addEventListener("click", () => { $("#graph-document-panel").open = true; $("#graph-document-id").focus(); $("#graph-document-panel").scrollIntoView({ behavior: "smooth", block: "start" }); });
  $("#graph-document-form").addEventListener("submit", saveGraphDocument);
  $("#graph-document-form").addEventListener("input", () => { state.graph.previewGeneration += 1; clear($("#graph-chunk-preview")); $("#graph-document-status").textContent = ""; });
  $("#graph-preview").addEventListener("click", previewGraphDocument);
  $("#graph-search-form").addEventListener("submit", retrieveGraphContext);
  $("#graph-filter-add").addEventListener("click", () => {
    if (state.graph.busy || state.graph.searchBusy || !graphCollection() || graphFilterDraft().length >= 32) return;
    graphFilterDraft().push({ column: "", dataType: "", operator: "eq", value: "" });
    renderGraphFilters(); graphFiltersChanged();
    $$("[data-graph-filter-row]").at(-1)?.querySelector("select").focus();
  });
  $("#graph-filter-clear").addEventListener("click", () => {
    if (state.graph.busy || state.graph.searchBusy) return;
    state.graph.filterDrafts.set(state.graph.collection, []); renderGraphFilters(); graphFiltersChanged();
  });
  $("#graph-candidates").addEventListener("input", updateGraphSeedBudget);
  $("#graph-hops").addEventListener("input", updateGraphSeedBudget);
  $("#graph-seeds").addEventListener("input", () => { state.graph.seedExplicit = true; updateGraphSeedBudget(); });
  $("#graph-seeds-auto").addEventListener("click", () => { state.graph.seedExplicit = false; updateGraphSeedBudget(); });
  updateGraphSeedBudget();
  $("#graph-reranker").addEventListener("change", () => { $("#graph-search-privacy").textContent = $("#graph-reranker").value === "voyage" ? "Your question is sent to the embedding provider. Voyage reranking also receives your question and candidate passage context, and may incur additional provider charges." : "Your question is sent to the embedding provider. Local ranking combines vector and lexical evidence on this server."; });
  $("#graph-relationship-form").addEventListener("submit", (event) => { event.preventDefault(); void mutateGraphRelationship("POST"); });
  $("#graph-previous").addEventListener("click", () => { if (!state.graph.busy && state.graph.mode === "pages") { state.graph.offset = Math.max(0, state.graph.offset - state.graph.limit); void loadGraph(); } });
  $("#graph-next").addEventListener("click", () => { if (!state.graph.busy && state.graph.mode === "pages") { state.graph.offset += state.graph.limit; void loadGraph(); } });
  $$("[data-graph-kind]").forEach((input) => input.addEventListener("change", applyGraphFilters));
  $("#graph-zoom-in").addEventListener("click", () => { state.graph.zoom = Math.min(2, state.graph.zoom + .25); applyGraphZoom(); });
  $("#graph-zoom-out").addEventListener("click", () => { state.graph.zoom = Math.max(.5, state.graph.zoom - .25); applyGraphZoom(); });
  $("#graph-zoom-reset").addEventListener("click", () => { state.graph.zoom = 1; state.graph.pan = { x: 0, y: 0 }; applyGraphZoom(); });
  const canvas = $("#graph-canvas");
  let drag = null;
  canvas.addEventListener("pointerdown", (event) => {
    if (event.button !== 0 || state.graph.zoom <= 1) return;
    state.graph.dragged = false;
    drag = { x: event.clientX, y: event.clientY, pan: { ...state.graph.pan } };
    canvas.setPointerCapture(event.pointerId);
  });
  canvas.addEventListener("pointermove", (event) => {
    if (!drag) return;
    const scale = 800 / canvas.getBoundingClientRect().width;
    const x = event.clientX - drag.x; const y = event.clientY - drag.y;
    if (Math.hypot(x, y) > 4) state.graph.dragged = true;
    state.graph.pan = { x: drag.pan.x + x * scale, y: drag.pan.y + y * scale }; applyGraphZoom();
  });
  const endDrag = () => { drag = null; window.setTimeout(() => { state.graph.dragged = false; }, 0); };
  canvas.addEventListener("pointerup", endDrag); canvas.addEventListener("pointercancel", endDrag);
  $("#settings-rerank-form").addEventListener("submit", saveRerankingSettings);
}
function resetGraphSession() {
  const uncertain = state.graph.busy;
  const settingsUncertain = state.rerankingBusy;
  state.graph = newGraphState();
  if (uncertain) state.graph.notice = "The connection changed during a graph request. It may still finish on the server; refresh and check its result before repeating it.";
  state.reranking = null; state.rerankingGeneration += 1; state.rerankingBusy = false;
  $("#rerank-api-key").value = ""; $("#rerank-clear-key").checked = false; $("#rerank-save").disabled = false;
  $("#rerank-key-status").textContent = "Reconnect to load reranking settings.";
  $("#rerank-settings-status").textContent = settingsUncertain ? "The connection changed while saving settings. The save may still finish; refresh to check the current configuration." : "";
  $("#graph-document-form").reset(); $("#graph-search-form").reset(); $("#graph-create-form").reset(); $("#graph-neighborhood-controls").reset();
  updateGraphSeedBudget();
  $("#graph-create-dialog").close();
  $("#data-create-dialog").close(); $("#admin-create-dialog").close();
  fieldBuilders.delete("admin-create-fields"); clear($("#admin-create-fields"));
  $("#admin-create-form").reset();
  mountFieldBuilder("graph-create-fields", [], { documentFields: true });
  renderGraphDocumentFields({ reset: true });
  renderGraphFilters();
  fillOptions($("#graph-collection"), [], null, "Choose a collection");
  for (const id of ["graph-chunk-preview", "graph-search-results"]) clear($("#" + id));
  for (const id of ["graph-document-status", "graph-search-status", "graph-create-status"]) $("#" + id).textContent = "";
  $("#graph-status").textContent = state.graph.notice || "Choose a collection after reconnecting.";
  graphProfileNote(); renderGraph(); renderGraphDetails(); setGraphBusy(false);
}

function bindWorkspaceEvents() {
  bindGraphEvents();
  $$('[data-go]').forEach((button) => button.addEventListener("click", () => switchView(button.dataset.go)));
  $$('[data-close-dialog]').forEach((button) => button.addEventListener("click", () => $("#" + button.dataset.closeDialog).close()));
  $("#search-mode-text").addEventListener("click", () => setSearchMode("text"));
  $("#search-mode-vector").addEventListener("click", () => setSearchMode("vector"));
  $("#settings-embedding-form").addEventListener("submit", saveEmbeddingSettings);
  $("#embedding-provider").addEventListener("change", () => { $("#embedding-api-key").value = ""; populateEmbeddingModels(); });
  $("#embedding-model").addEventListener("change", () => updateEmbeddingDimensions());
  $("#settings-refresh").addEventListener("click", loadSettingsView);
  $("#settings-browser-form").addEventListener("submit", saveBrowserPreferences);
  $("#settings-token").addEventListener("click", () => $("#token-dialog").showModal());
  $("#admin-table").addEventListener("change", (event) => void selectAdminTable(event.target.value));
  $("#admin-refresh").addEventListener("click", () => void loadAdminRows());
  $("#admin-page-size").addEventListener("change", (event) => { state.admin.limit = Number(event.target.value); state.admin.offset = 0; void loadAdminRows(); });
  $("#admin-previous").addEventListener("click", () => { state.admin.offset = Math.max(0, state.admin.offset - state.admin.limit); void loadAdminRows(); });
  $("#admin-next").addEventListener("click", () => { state.admin.offset += state.admin.limit; void loadAdminRows(); });
  $("#admin-new-row").addEventListener("click", () => {
    if (state.admin.busy) return;
    resetRowEditor();
    $("#admin-row-json").focus({ preventScroll: true });
    $("#admin-editor-title").scrollIntoView({ behavior: "smooth", block: "nearest" });
  });
  $("#admin-clear-selection").addEventListener("click", resetRowEditor);
  $("#admin-add-row").addEventListener("click", insertAdminRow);
  $("#admin-save-row").addEventListener("click", saveAdminRow);
  $("#admin-delete-row").addEventListener("click", openDeleteRow);
  $("#admin-delete-form").addEventListener("submit", deleteAdminRow);
  $("#admin-embed-insert").addEventListener("click", embedAndInsertDocuments);
  $("#admin-clear-documents").addEventListener("click", () => { if (!state.admin.busy) { $("#admin-documents-json").value = ""; $("#admin-ingest-status").textContent = ""; } });
  $("#admin-new-table").addEventListener("click", () => {
    if (state.admin.busy || state.graph.busy) return;
    $("#data-create-dialog").showModal();
  });
  $("#data-create-collection").addEventListener("click", openCreateCollection);
  $("#data-create-table").addEventListener("click", openCreateTable);
  $("#admin-create-form").addEventListener("submit", createAdminTable);
  $("#relationship-new").addEventListener("click", openRelationshipDialog);
  $("#relationships-refresh").addEventListener("click", () => { state.relationships.notice = ""; void loadRelationships(); });
  $("#relationship-dialog-refresh").addEventListener("click", async () => {
    if (state.relationships.busy) return;
    const session = state.session;
    if (await loadRelationships() && session === state.session && $("#relationship-dialog").open) { invalidateSchemas(); await loadRelationshipColumns(); }
  });
  $("#relationship-form").addEventListener("submit", saveRelationship);
  $("#relationship-dialog").addEventListener("close", () => { state.relationships.schemaGeneration += 1; });
  for (const id of ["relationship-source-table", "relationship-target-table"]) $("#" + id).addEventListener("change", () => void loadRelationshipColumns());
  for (const id of ["relationship-source-column", "relationship-target-column"]) $("#" + id).addEventListener("change", () => updateRelationshipTargets());
  $("#relationship-name").addEventListener("input", () => updateRelationshipTargets());
  $("#admin-drop-table").addEventListener("click", openDropTable);
  $("#admin-drop-confirm").addEventListener("input", () => { $("#admin-drop-submit").disabled = $("#admin-drop-confirm").value !== state.admin.dropTarget?.table; });
  $("#admin-drop-form").addEventListener("submit", dropAdminTable);
}

function bindEvents() {
  bindWorkspaceEvents();
  $("#token-dialog .dialog-close").addEventListener("click", () => $("#token-dialog").close());
  $$(".nav-item").forEach((button) => button.addEventListener("click", () => switchView(button.dataset.view)));
  $("#refresh-tables").addEventListener("click", () => refreshConnection({ force: true }));
  $("#reconnect").addEventListener("click", () => refreshConnection({ force: true }));
  $("#mobile-menu").addEventListener("click", () => setSidebarOpen(!document.body.classList.contains("sidebar-open")));
  $("#connection-button").addEventListener("click", () => $("#token-dialog").showModal());
  $("#open-token").addEventListener("click", () => $("#token-dialog").showModal());
  $("#top-guide").addEventListener("click", () => openGuide());
  $("#quick-start-guide").addEventListener("click", () => openGuide("#tutorial-title"));
  $("#dismiss-quick-start").addEventListener("click", dismissQuickStart);
  $("#token-form").addEventListener("submit", (event) => {
    if (event.submitter?.value === "cancel") return;
    event.preventDefault();
    changeToken($("#token-input").value.trim());
  });
  $("#clear-token").addEventListener("click", () => changeToken(""));
  $("#run-sql").addEventListener("click", runSql);
  $("#analyze-sql").addEventListener("click", analyzeSql);
  $("#example-select").addEventListener("change", (event) => setEditor(examples[event.target.value]));
  $("#format-sql").addEventListener("click", () => setEditor($("#sql-editor").value.trim().replace(/\n{3,}/g, "\n\n")));
  $("#sql-editor").addEventListener("input", updateLineNumbers);
  $("#sql-editor").addEventListener("scroll", () => { $("#line-numbers").scrollTop = $("#sql-editor").scrollTop; });
  $("#sql-editor").addEventListener("keydown", (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key === "Enter") {
      event.preventDefault();
      runSql();
    }
    if (event.key === "Tab") {
      event.preventDefault();
      const editor = event.target;
      const start = editor.selectionStart;
      editor.setRangeText("  ", start, editor.selectionEnd, "end");
      updateLineNumbers();
    }
  });
  $("#search-table").addEventListener("change", (event) => { state.searchIntentGeneration += 1; void populateSearchColumns(event.target.value); });
  $("#search-vector-column").addEventListener("change", () => { state.searchIntentGeneration += 1; updateDimensionHint(); });
  $("#search-form").addEventListener("submit", runVectorSearch);
  $$('[data-load-example]').forEach((button) => button.addEventListener("click", () => {
    setEditor(examples[button.dataset.loadExample]);
    $("#example-select").value = button.dataset.loadExample;
    switchView("console");
    $("#sql-editor").focus();
  }));
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && document.body.classList.contains("sidebar-open")) {
      setSidebarOpen(false);
      $("#mobile-menu").focus();
      return;
    }
    const target = event.target;
    const isEditing = target instanceof HTMLElement
      && (target.matches("input, textarea, select") || target.isContentEditable);
    if (event.key === "?" && !event.altKey && !event.ctrlKey && !event.metaKey && !isEditing && !$("dialog[open]")) {
      event.preventDefault();
      openGuide("#command-reference-title");
    }
  });
}

function changeToken(token) {
  const interrupted = state.sqlBusy;
  const interruptedAdmin = state.admin.busy;
  clear($("#toast-region"));
  state.session += 1;
  resetGraphSession();
  resetRelationshipsSession();
  state.embeddingGeneration += 1;
  state.embeddingRequest = null;
  state.embeddings = null;
  state.searchIntentGeneration += 1;
  state.admin.generation += 1;
  state.admin.table = "";
  state.admin.rows = [];
  state.admin.selected = null;
  state.admin.busy = false;
  state.embeddingSaveBusy = false;
  $("#embedding-save").disabled = false;
  $("#embedding-api-key").value = "";
  $("#admin-tools").hidden = true;
  clear($("#admin-rows")).append(node("div", "empty-workspace", "Choose a table after reconnecting."));
  state.token = token;
  $("#admin-status").textContent = interruptedAdmin
    ? "The connection changed during a data request. It may still finish on the server; check the data before repeating it."
    : "Choose a table after reconnecting.";
  for (const controller of state.requests) controller.abort();
  state.tablesGeneration += 1;
  state.tablesRequest = null;
  state.inspectorGeneration += 1;
  state.searchColumnsGeneration += 1;
  invalidateSchemas();
  state.revision = null;
  state.tables = [];
  state.activeTable = null;
  setSqlBusy(false, interrupted ? "Previous request may still finish" : "Ready");
  if (interrupted) {
    renderResultError(new Error("The connection changed while a request was running. It may still finish on the server; check its result before running it again."));
    $("#results-title").textContent = "Request interrupted";
  }
  state.inspectorFailed = false;
  state.searchColumnsFailed = false;
  state.searchBusy = false;
  $("#search-form button[type=submit]").disabled = false;
  $("#search-results").setAttribute("aria-busy", "false");
  clear($("#table-inspector")).append(node("div", "empty-inspector", "Select a table to inspect its columns."));
  renderTableList();
  updateStats({ tables: [] });
  populateSearchTables();
  void populateSearchColumns("");
  try {
    if (token) sessionStorage.setItem("vectors.apiToken", token);
    else sessionStorage.removeItem("vectors.apiToken");
  } catch { toast("Browser storage is unavailable. The token will be kept only until this page reloads.", "error"); }
  $("#token-input").value = token;
  $("#token-dialog").close();
  void refreshConnection({ force: true });
  void loadEmbeddingSettings({ form: state.view === "settings" }).catch(() => {});
  if (state.view === "settings") { void loadServerSettings(); void loadRerankingSettings().catch(() => {}); }
  if (state.view === "connections") void loadGraphCollections();
  if (state.view === "data") void loadRelationships();
}

async function initialize() {
  bindEvents();
  $("#token-input").value = state.token;
  $("#connection-host").textContent = window.location.host;
  $("#connection-host").title = window.location.origin;
  setEditor(examples.quickstart);
  switchView("search");
  setSearchMode("text");
  revealQuickStart();
  applyBrowserPreferences();
  void loadEmbeddingSettings().catch(() => {});
  window.addEventListener("online", () => refreshConnection({ quiet: true }));
  window.addEventListener("offline", () => setConnection("offline", "Disconnected"));
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) void refreshConnection({ quiet: true });
  });
  await refreshConnection();
}

initialize();
