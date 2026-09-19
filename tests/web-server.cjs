"use strict";

const http = require("node:http");
const fs = require("node:fs/promises");
const path = require("node:path");

// Browser tests mock the API and serve the exact console assets embedded in Rust.
const assets = new Map([
  ["/", ["index.html", "text/html; charset=utf-8"]],
  ["/assets/app.js", ["app.js", "text/javascript; charset=utf-8"]],
  ["/assets/app.css", ["app.css", "text/css; charset=utf-8"]],
]);

const server = http.createServer(async (request, response) => {
  const asset = assets.get(new URL(request.url, "http://localhost").pathname);
  if (!asset) {
    response.writeHead(404).end("Not found");
    return;
  }
  try {
    const content = await fs.readFile(path.join(__dirname, "../web", asset[0]));
    response.writeHead(200, { "content-type": asset[1], "cache-control": "no-store" });
    response.end(content);
  } catch {
    response.writeHead(500).end("Unable to load console asset");
  }
});

server.listen(Number(process.env.PORT || 4173), "127.0.0.1");
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => server.close(() => process.exit(0)));
}
