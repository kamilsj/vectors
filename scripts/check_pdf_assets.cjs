"use strict";
// Verify the offline bundle matches the exact npm lockfile dependency.
const fs = require("node:fs");
const path = require("node:path");
const root = path.resolve(__dirname, "..");
const source = path.join(root, "node_modules/pdfjs-dist");
const target = path.join(root, "web/vendor/pdfjs");
const version = JSON.parse(fs.readFileSync(path.join(source, "package.json"))).version;
const expected = new Map([
  ["pdf.mjs", "build/pdf.min.mjs"],
  ["pdf.worker.mjs", "build/pdf.worker.min.mjs"],
  ["LICENSE", "LICENSE"],
]);
function walk(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const name = path.join(directory, entry.name);
    return entry.isDirectory() ? walk(name) : [name];
  });
}
for (const folder of ["cmaps", "standard_fonts"]) {
  for (const file of walk(path.join(source, folder))) {
    const relative = path.relative(source, file);
    expected.set(relative, relative);
  }
}
for (const [bundled, original] of expected) {
  if (!fs.readFileSync(path.join(target, bundled)).equals(fs.readFileSync(path.join(source, original)))) {
    throw new Error(`Bundled PDF asset differs from pdfjs-dist ${version}: ${bundled}`);
  }
}
for (const file of walk(target)) {
  const relative = path.relative(target, file);
  if (relative !== "NOTICE.md" && !expected.has(relative)) throw new Error(`Unexpected PDF asset: ${relative}`);
}
if (!fs.readFileSync(path.join(target, "NOTICE.md"), "utf8").includes(`**${version}**`)) {
  throw new Error("Update PDF asset provenance to match the pinned dependency");
}
console.log(`Verified ${expected.size} bundled PDF assets against pdfjs-dist ${version}.`);
