"use strict";

const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => Array.from(root.querySelectorAll(selector));

const state = {
  token: sessionStorage.getItem("vectors.apiToken") || "",
  tables: [],
  schemas: new Map(),
  activeTable: null,
  view: "console",
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

async function request(path, options = {}) {
  const headers = new Headers(options.headers || {});
  headers.set("accept", "application/json");
  if (options.body) headers.set("content-type", "application/json");
  if (state.token) headers.set("authorization", `Bearer ${state.token}`);
  const response = await fetch(path, { ...options, headers });
  const type = response.headers.get("content-type") || "";
  const payload = type.includes("application/json") ? await response.json() : null;
  if (!response.ok) {
    const error = new Error(payload?.error?.message || `${response.status} ${response.statusText}`);
    error.status = response.status;
    error.code = payload?.error?.code;
    throw error;
  }
  return payload;
}

function setConnection(status, label) {
  const dot = $("#status-dot");
  dot.className = `status-dot ${status}`;
  $("#status-label").textContent = label;
}

function toast(message, kind = "success") {
  const item = node("div", `toast ${kind === "error" ? "error" : ""}`, message);
  $("#toast-region").append(item);
  window.setTimeout(() => item.remove(), 4200);
}

function showError(error) {
  setConnection(error.status === 401 ? "offline" : "offline", error.status === 401 ? "Token required" : "Disconnected");
  if (error.status === 401) {
    toast("Authentication required. Add the server API token.", "error");
    $("#token-dialog").showModal();
  } else {
    toast(error.message || String(error), "error");
  }
}

async function loadTables({ quiet = false } = {}) {
  try {
    if (!quiet) setConnection("", "Connecting");
    const data = await request("/v1/tables");
    state.tables = data.tables;
    renderTableList();
    updateStats(data);
    populateSearchTables();
    setConnection("online", "Connected");
    return data;
  } catch (error) {
    state.tables = [];
    renderTableList();
    updateStats({ revision: null, tables: [] });
    showError(error);
    return null;
  }
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
    list.append(node("div", "sidebar-empty", "No tables yet — run the quickstart"));
    return;
  }
  state.tables.forEach((table) => {
    const button = node("button", `table-button ${state.activeTable === table.name ? "active" : ""}`);
    button.type = "button";
    button.dataset.table = table.name;
    button.append(node("span", "table-glyph", "▦"));
    button.append(node("span", "table-name", table.name));
    button.append(node("small", "", table.row_count.toLocaleString()));
    button.addEventListener("click", () => inspectTable(table.name));
    list.append(button);
  });
}

async function inspectTable(tableName) {
  state.activeTable = tableName;
  renderTableList();
  switchView("console");
  const inspector = clear($("#table-inspector"));
  const loading = node("div", "empty-inspector");
  loading.append(node("div", "empty-icon", "···"), node("p", "", `Loading ${tableName}`));
  inspector.append(loading);
  try {
    const encoded = encodeURIComponent(tableName);
    const [schema, indexes] = await Promise.all([
      request(`/v1/tables/${encoded}/schema`),
      request(`/v1/tables/${encoded}/indexes`),
    ]);
    state.schemas.set(tableName, schema.columns);
    renderInspector(tableName, schema.columns, indexes.indexes);
  } catch (error) {
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
  $$(".nav-item").forEach((element) => element.classList.toggle("active", element.dataset.view === view));
  const titles = { console: "SQL console", search: "Vector search", guide: "Start here" };
  $("#view-title").textContent = titles[view];
  document.body.classList.remove("sidebar-open");
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

async function runSql() {
  const sql = $("#sql-editor").value.trim();
  if (!sql) {
    toast("Write or load a SQL statement first.", "error");
    return;
  }
  const button = $("#run-sql");
  const started = performance.now();
  button.disabled = true;
  $("#editor-status").textContent = "Running…";
  try {
    const data = await request("/v1/sql", { method: "POST", body: JSON.stringify({ sql }) });
    const elapsed = performance.now() - started;
    renderSqlResults(data.results, elapsed);
    $("#editor-status").textContent = "Complete";
    setConnection("online", "Connected");
    await loadTables({ quiet: true });
    if (state.activeTable && state.tables.some((table) => table.name === state.activeTable)) {
      await inspectTable(state.activeTable);
    }
  } catch (error) {
    $("#editor-status").textContent = "Error";
    renderResultError(error);
    showError(error);
  } finally {
    button.disabled = false;
  }
}

async function analyzeSql() {
  const sql = $("#sql-editor").value.trim();
  if (!sql) {
    toast("Write or load a SELECT statement first.", "error");
    return;
  }
  const button = $("#analyze-sql");
  button.disabled = true;
  $("#editor-status").textContent = "Understanding…";
  try {
    const intent = await request("/v1/sql/intent", { method: "POST", body: JSON.stringify({ sql }) });
    renderQueryIntent(intent);
    $("#editor-status").textContent = "Intent ready";
  } catch (error) {
    $("#editor-status").textContent = "Error";
    renderResultError(error);
    showError(error);
  } finally {
    button.disabled = false;
  }
}

function renderQueryIntent(intent) {
  const target = clear($("#sql-results"));
  target.className = "";
  $("#results-title").textContent = "Query intent";
  const metrics = clear($("#query-metrics"));
  metrics.append(node("span", "metric-chip", intent.operation.toUpperCase()));
  if (intent.table) metrics.append(node("span", "metric-chip", intent.table));
  if (intent.vector_search?.optimized) metrics.append(node("span", "metric-chip", "VectorTopK"));

  const summary = node("div", "intent-summary");
  summary.append(node("span", "panel-kicker", "SCHEMA-AWARE INTERPRETATION"), node("h3", "", intent.summary));
  const details = node("div", "intent-details");
  if (intent.filter) details.append(node("span", "", `Filter · ${intent.filter}`));
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
  block.append(renderDataTable(result.columns, result.rows));
  const meta = node("div", "result-meta");
  meta.append(node("span", "", `${result.row_count} row(s)`), node("span", "", `${result.rows_examined} examined`));
  block.append(meta);
  return block;
}

function renderDataTable(columns, rows) {
  const wrap = node("div", "data-table-wrap");
  const table = node("table", "data-table");
  const head = document.createElement("thead");
  const headRow = document.createElement("tr");
  columns.forEach((column) => headRow.append(node("th", "", column)));
  head.append(headRow);
  const body = document.createElement("tbody");
  rows.forEach((row) => {
    const tableRow = document.createElement("tr");
    row.forEach((value) => {
      const cell = document.createElement("td");
      if (value === null) {
        cell.className = "null-value";
        cell.textContent = "NULL";
      } else if (Array.isArray(value)) {
        cell.className = "vector-value";
        cell.textContent = `[${value.slice(0, 10).join(", ")}${value.length > 10 ? ", …" : ""}]`;
        cell.title = JSON.stringify(value);
      } else if (typeof value === "object") {
        cell.textContent = JSON.stringify(value);
      } else {
        cell.textContent = String(value);
      }
      tableRow.append(cell);
    });
    body.append(tableRow);
  });
  table.append(head, body);
  wrap.append(table);
  return wrap;
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
  const vectorSelect = $("#search-vector-column");
  const filterSelect = $("#filter-column");
  clear(vectorSelect).append(node("option", "", "Choose a vector"));
  vectorSelect.firstElementChild.value = "";
  clear(filterSelect).append(node("option", "", "No filter"));
  filterSelect.firstElementChild.value = "";
  if (!tableName) return;
  try {
    let columns = state.schemas.get(tableName);
    if (!columns) {
      const schema = await request(`/v1/tables/${encodeURIComponent(tableName)}/schema`);
      columns = schema.columns;
      state.schemas.set(tableName, columns);
    }
    columns.forEach((column) => {
      if (column.data_type.startsWith("VECTOR")) {
        const option = node("option", "", `${column.name} · ${column.data_type}`);
        option.value = column.name;
        option.dataset.type = column.data_type;
        vectorSelect.append(option);
      } else {
        const option = node("option", "", `${column.name} · ${column.data_type}`);
        option.value = column.name;
        option.dataset.type = column.data_type;
        filterSelect.append(option);
      }
    });
    const scalarNames = columns.filter((column) => !column.data_type.startsWith("VECTOR")).map((column) => column.name);
    $("#search-select").value = scalarNames.slice(0, 5).join(", ");
    if (vectorSelect.options.length === 2) {
      vectorSelect.selectedIndex = 1;
      updateDimensionHint();
    }
  } catch (error) {
    showError(error);
  }
}

function updateDimensionHint() {
  const option = $("#search-vector-column").selectedOptions[0];
  const match = option?.dataset.type?.match(/VECTOR\((\d+)\)/);
  if (!match) {
    $("#dimension-hint").textContent = "Select a vector column to see its dimensions.";
    return;
  }
  const dimensions = Number(match[1]);
  $("#dimension-hint").textContent = `Expected dimensions: ${dimensions}`;
  const vectorInput = $("#search-vector");
  if (!vectorInput.value.trim()) {
    vectorInput.value = Array.from({ length: dimensions }, (_, index) => index === 0 ? "1" : "0").join(", ");
  }
}

function parseFilterValue(value, dataType) {
  if (dataType === "BOOLEAN") return value.toLowerCase() === "true";
  if (dataType === "INTEGER" || dataType === "DOUBLE") {
    const number = Number(value);
    if (!Number.isFinite(number)) throw new Error(`Filter value must be numeric for ${dataType}`);
    return number;
  }
  return value;
}

async function runVectorSearch(event) {
  event.preventDefault();
  const table = $("#search-table").value;
  const vectorColumn = $("#search-vector-column").value;
  const query = $("#search-vector").value.split(",").map((value) => Number(value.trim()));
  if (!query.length || query.some((value) => !Number.isFinite(value))) {
    toast("Query vector must contain finite comma-separated numbers.", "error");
    return;
  }
  const payload = {
    table,
    vector_column: vectorColumn,
    query,
    metric: $("#search-metric").value,
    select: $("#search-select").value.split(",").map((value) => value.trim()).filter(Boolean),
    limit: Number($("#search-limit").value),
    filters: [],
  };
  const filterColumn = $("#filter-column").value;
  const filterValue = $("#filter-value").value;
  if (filterColumn) {
    const selected = $("#filter-column").selectedOptions[0];
    payload.filters.push({
      column: filterColumn,
      operator: $("#filter-operator").value,
      value: parseFilterValue(filterValue, selected.dataset.type),
    });
  }

  const button = $("#search-form button[type=submit]");
  button.disabled = true;
  const started = performance.now();
  try {
    const result = await request("/v1/vector/search", { method: "POST", body: JSON.stringify(payload) });
    renderSearchResult(result, performance.now() - started);
    setConnection("online", "Connected");
  } catch (error) {
    const target = clear($("#search-results"));
    target.className = "results-empty";
    target.append(node("p", "", error.message));
    $("#search-results-title").textContent = "Search failed";
    showError(error);
  } finally {
    button.disabled = false;
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

function bindEvents() {
  $$(".nav-item").forEach((button) => button.addEventListener("click", () => switchView(button.dataset.view)));
  $("#refresh-tables").addEventListener("click", () => loadTables());
  $("#mobile-menu").addEventListener("click", () => document.body.classList.toggle("sidebar-open"));
  $("#connection-button").addEventListener("click", () => $("#token-dialog").showModal());
  $("#open-token").addEventListener("click", () => $("#token-dialog").showModal());
  $("#top-guide").addEventListener("click", () => switchView("guide"));
  $("#save-token").addEventListener("click", async () => {
    state.token = $("#token-input").value.trim();
    sessionStorage.setItem("vectors.apiToken", state.token);
    $("#token-dialog").close();
    await loadTables();
  });
  $("#clear-token").addEventListener("click", async () => {
    state.token = "";
    sessionStorage.removeItem("vectors.apiToken");
    $("#token-input").value = "";
    $("#token-dialog").close();
    await loadTables();
  });
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
  $("#search-table").addEventListener("change", (event) => populateSearchColumns(event.target.value));
  $("#search-vector-column").addEventListener("change", updateDimensionHint);
  $("#search-form").addEventListener("submit", runVectorSearch);
  $$('[data-load-example]').forEach((button) => button.addEventListener("click", () => {
    setEditor(examples[button.dataset.loadExample]);
    $("#example-select").value = button.dataset.loadExample;
    switchView("console");
    $("#sql-editor").focus();
  }));
}

async function initialize() {
  bindEvents();
  $("#token-input").value = state.token;
  setEditor(examples.quickstart);
  try {
    const health = await request("/healthz");
    $("#app-version").textContent = health.version;
    $("#storage-mode-label").textContent = health.storage === "durable" ? "WAL + checkpoint" : "in memory";
    await loadTables();
  } catch (error) {
    showError(error);
  }
}

initialize();
