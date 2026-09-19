#!/usr/bin/env node
"use strict";

// Start `node tests/web-server.cjs`, then run this file. Measurements isolate
// browser result rendering: API transfer, JSON parsing, and data generation
// are deliberately outside the timed section.
const fs = require("node:fs/promises");
const path = require("node:path");
const os = require("node:os");

function parseOptions(args) {
  const options = { rows: 10_000, dimensions: 384, repetitions: 5, warmups: 1, seed: 42, format: "json" };
  for (let index = 0; index < args.length; index += 1) {
    const flag = args[index];
    if (flag === "--help") {
      return null;
    }
    const key = flag.replace(/^--/, "");
    if (!flag.startsWith("--") || !["baseline", ...Object.keys(options)].includes(key)) {
      throw new Error(`Unknown option: ${flag}`);
    }
    const value = args[++index];
    if (value === undefined) throw new Error(`Missing value for ${flag}`);
    options[key] = ["baseline", "format"].includes(key) ? value : Number(value);
  }
  for (const key of ["rows", "dimensions", "repetitions", "warmups", "seed"]) {
    if (!Number.isSafeInteger(options[key]) || options[key] < (key === "seed" ? 0 : 1)) {
      throw new Error(`--${key} must be a ${key === "seed" ? "nonnegative" : "positive"} integer`);
    }
  }
  if (options.seed > 0xffffffff) throw new Error("--seed must fit in an unsigned 32-bit integer");
  if (!["json", "csv"].includes(options.format)) throw new Error("--format must be json or csv");
  return options;
}

const median = (values) => {
  const sorted = [...values].sort((left, right) => left - right);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
};

async function benchmarkVariant(browser, name, source, options, baseURL) {
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 }, locale: "en-US" });
  try {
    const page = await context.newPage();
    const pageErrors = [];
    page.on("pageerror", (error) => pageErrors.push(error.message));
    await page.route("**/healthz", (route) => route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({ status: "ok", version: "benchmark", storage: "memory" }),
    }));
    await page.route("**/v1/**", (route) => {
      const pathname = new URL(route.request().url()).pathname;
      const response = pathname === "/v1/tables" ? { revision: 0, tables: [] }
        : pathname === "/v1/settings/embeddings" ? {
          provider: "openai", model: "text-embedding-3-small", dimensions: 1536,
          configured: false, persistence: "memory", batch_size: 32,
          timeout_seconds: 60, max_concurrent_requests: 4,
          providers: [{ id: "openai", label: "OpenAI", configured: false,
            models: [{ id: "text-embedding-3-small", dimensions: [1536], default_dimensions: 1536 }] }],
        } : null;
      return route.fulfill({
        status: response ? 200 : 404, contentType: "application/json",
        body: JSON.stringify(response || { error: { code: "unexpected_benchmark_request", message: pathname } }),
      });
    });
    if (source !== null) {
      await page.route("**/assets/app.js", (route) => route.fulfill({
        contentType: "text/javascript; charset=utf-8", body: source,
      }));
    }
    await page.goto(baseURL, { waitUntil: "networkidle" });
    await page.waitForFunction(() => document.querySelector("#status-label")?.textContent === "Connected");
    await page.evaluate(({ rows, dimensions, seed }) => {
      // Search is the workspace's default view; hidden tables skip layout work.
      switchView("console");
      let randomState = seed;
      const data = Array.from({ length: rows }, (_, index) => [
        index + 1,
        `Document ${String(index + 1).padStart(6, "0")}`,
        Array.from({ length: dimensions }, () => {
          randomState = (Math.imul(randomState, 1664525) + 1013904223) >>> 0;
          return ((randomState % 20001) - 10000) / 10000;
        }),
      ]);
      window.__vectorsRenderBenchmark = {
        columns: ["id", "title", "embedding"], rows: data,
        schema: [{ data_type: "INTEGER" }, { data_type: "TEXT" }, { data_type: `VECTOR(${dimensions})` }],
      };
      document.querySelector("#quick-start").hidden = true;
      document.querySelector("#sql-results").className = "";
    }, options);

    const render = () => page.evaluate(() => {
      const data = window.__vectorsRenderBenchmark;
      const target = document.querySelector("#sql-results");
      const started = performance.now();
      target.replaceChildren(renderDataTable(data.columns, data.rows, data.schema));
      // Reading geometry forces the browser to finish table style and layout.
      const renderedHeight = target.getBoundingClientRect().height;
      const renderMs = performance.now() - started;
      if (renderedHeight <= 0) throw new Error("Benchmark result table is hidden; layout was not measured");
      const walker = document.createTreeWalker(target, NodeFilter.SHOW_ALL);
      let domNodes = 0;
      while (walker.nextNode()) domNodes += 1;
      return {
        render_ms: Number(renderMs.toFixed(3)),
        rendered_rows: target.querySelector("tbody").rows.length,
        dom_nodes: domNodes,
        dom_elements: target.querySelectorAll("*").length,
        rendered_height_px: renderedHeight,
      };
    });
    const clear = async () => {
      await page.evaluate(() => {
        document.querySelector("#sql-results").replaceChildren();
        return document.body.offsetHeight;
      });
      // Allow cleanup and painting between samples, outside the measured time.
      await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    };

    // Verify equivalent previews and complete values before collecting samples.
    await render();
    const verification = await page.evaluate(({ variant, rowCount }) => {
      const data = window.__vectorsRenderBenchmark;
      const target = document.querySelector("#sql-results");
      const displayed = target.querySelector("tbody").rows;
      const expectedCount = variant === "baseline" ? rowCount : Math.min(100, rowCount);
      if (displayed.length !== expectedCount) {
        throw new Error(`${variant}: rendered ${displayed.length} rows; expected ${expectedCount}`);
      }
      const count = Math.min(100, rowCount);
      for (let index = 0; index < count; index += 1) {
        const cells = displayed[index].cells;
        const expected = data.rows[index];
        if (cells[0].textContent !== String(expected[0]) || cells[1].textContent !== expected[1]) {
          throw new Error(`${variant}: scalar mismatch at row ${index + 1}`);
        }
        const vector = expected[2];
        const preview = `[${vector.slice(0, 10).join(", ")}${vector.length > 10 ? ", …" : ""}]`;
        const actual = cells[2].querySelector(".cell-value-preview")?.textContent || cells[2].textContent;
        if (actual !== preview) throw new Error(`${variant}: vector preview mismatch at row ${index + 1}`);
      }
      const vectorCell = displayed[0].cells[2];
      const toggle = vectorCell.querySelector("button");
      if (toggle) {
        if (vectorCell.querySelector(".cell-value-full").textContent !== "") {
          throw new Error(`${variant}: full vector was created before expansion`);
        }
        toggle.click();
      }
      const fullValue = vectorCell.querySelector(".cell-value-full")?.textContent
        || vectorCell.title || vectorCell.textContent;
      if (JSON.stringify(JSON.parse(fullValue)) !== JSON.stringify(data.rows[0][2])) {
        throw new Error(`${variant}: complete vector differs from the source data`);
      }
      if (toggle) {
        toggle.click();
        if (vectorCell.querySelector(".cell-value-full").textContent !== "") {
          throw new Error(`${variant}: collapsed vector remains in the DOM`);
        }
      }
      return { preview_rows_verified: count, full_vector_verified: true };
    }, { variant: name, rowCount: options.rows });
    await clear();

    for (let index = 0; index < options.warmups; index += 1) {
      await render();
      await clear();
    }
    const samples = [];
    for (let index = 0; index < options.repetitions; index += 1) {
      samples.push(await render());
      await clear();
    }
    if (pageErrors.length) throw new Error(`${name}: browser errors: ${pageErrors.join("; ")}`);
    return {
      variant: name,
      median_render_ms: Number(median(samples.map((sample) => sample.render_ms)).toFixed(3)),
      rendered_rows: samples[0].rendered_rows,
      dom_nodes: samples[0].dom_nodes,
      dom_elements: samples[0].dom_elements,
      ...verification,
      samples,
    };
  } finally {
    await context.close();
  }
}

async function main() {
  const options = parseOptions(process.argv.slice(2));
  if (!options) {
    process.stdout.write("Usage: node scripts/benchmark_web.cjs [--baseline old-app.js] [--rows 10000] [--dimensions 384] [--repetitions 5] [--warmups 1] [--seed 42] [--format json|csv]\nStart node tests/web-server.cjs first. VECTORS_WEB_URL and PLAYWRIGHT_CHROMIUM_EXECUTABLE are optional.\n");
    return;
  }
  const baseURL = process.env.VECTORS_WEB_URL || "http://127.0.0.1:4173";
  const baseline = options.baseline ? await fs.readFile(path.resolve(options.baseline), "utf8") : null;
  const { chromium } = require("@playwright/test");
  const browser = await chromium.launch({
    headless: true,
    ...(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE
      ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {}),
  });
  try {
    const results = [];
    if (baseline !== null) results.push(await benchmarkVariant(browser, "baseline", baseline, options, baseURL));
    results.push(await benchmarkVariant(browser, "current", null, options, baseURL));
    const report = {
      benchmark: "web_result_render",
      measured_at: new Date().toISOString(),
      browser_version: browser.version(),
      platform: `${os.platform()} ${os.arch()}`,
      cpu: os.cpus()[0]?.model,
      viewport: { width: 1440, height: 1000 },
      row_count: options.rows,
      dimensions: options.dimensions,
      seed: options.seed,
      warmups: options.warmups,
      repetitions: options.repetitions,
      timing_scope: "DOM construction, insertion, style, and forced layout; excludes data generation, API transfer, and JSON parsing",
      results,
    };
    if (results.length === 2) {
      report.median_speedup = Number((results[0].median_render_ms / results[1].median_render_ms).toFixed(3));
    }
    if (options.format === "csv") {
      const fields = ["variant", "row_count", "dimensions", "seed", "repetitions", "median_render_ms", "rendered_rows", "dom_nodes", "dom_elements", "browser_version"];
      const quote = (value) => `"${String(value).replaceAll('"', '""')}"`;
      process.stdout.write(`${fields.join(",")}\n`);
      for (const result of results) {
        process.stdout.write(`${fields.map((field) => quote(result[field] ?? report[field])).join(",")}\n`);
      }
    } else {
      process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
    }
  } finally {
    await browser.close();
  }
}

main().catch((error) => {
  process.stderr.write(`${error.stack || error.message}\n`);
  process.exitCode = 1;
});
