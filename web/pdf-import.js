// Local PDF extraction. Original PDF bytes never leave the browser.
export const PDF_IMPORT_LIMITS = Object.freeze({ fileBytes: 50 * 1024 * 1024, queueFiles: 10000, partBytes: 64 * 1024, pageTextBytes: 16 * 1024 * 1024, pages: 10000 });
const encoder = new TextEncoder();
let pdfLibrary;
const aborted = () => new DOMException("PDF processing stopped", "AbortError");
function check(signal) { if (signal?.aborted) throw aborted(); }
export function pdfRelativePath(file) { return file.webkitRelativePath || file.name; }
export function validatePdfFile(file) {
  if (!/\.pdf$/i.test(file.name)) throw new Error("Only PDF files can be imported.");
  if (!file.size) throw new Error("The PDF is empty.");
  if (file.size > PDF_IMPORT_LIMITS.fileBytes) throw new Error("PDF exceeds 50 MiB. Split it before importing.");
}
export function splitPdfText(text, maxBytes = PDF_IMPORT_LIMITS.partBytes) {
  const parts = []; let current = []; let size = 0;
  for (const character of text) {
    const bytes = encoder.encode(character).length;
    if (size + bytes > maxBytes && current.length) { parts.push(current.join("")); current = []; size = 0; }
    current.push(character); size += bytes;
  }
  if (current.length) parts.push(current.join(""));
  return parts;
}
export function truncatePdfLabel(text, maxBytes = 700) {
  let result = ""; let size = 0;
  for (const character of text) { const bytes = encoder.encode(character).length; if (size + bytes > maxBytes) break; result += character; size += bytes; }
  return result;
}
async function digest(data) {
  if (!globalThis.crypto?.subtle) throw new Error("PDF import needs a secure browser connection. Use localhost or HTTPS.");
  return Array.from(new Uint8Array(await crypto.subtle.digest("SHA-256", data)), (byte) => byte.toString(16).padStart(2, "0")).join("");
}
export async function pdfDocumentId(fingerprint, page, part) {
  return `pdf_${await digest(encoder.encode(`vectors-pdf-v1\n${fingerprint}\n${page}\n${part}`))}`;
}
export async function* extractPdfPages(file, { signal, onLoad = () => {} } = {}) {
  validatePdfFile(file); check(signal);
  const data = new Uint8Array(await file.arrayBuffer()); check(signal);
  const fingerprint = await digest(encoder.encode(`${pdfRelativePath(file)}\n${await digest(data)}`)); check(signal);
  pdfLibrary ||= import("./vendor/pdfjs/pdf.mjs");
  const pdfjs = await pdfLibrary; check(signal);
  pdfjs.GlobalWorkerOptions.workerSrc = new URL("./vendor/pdfjs/pdf.worker.mjs", import.meta.url).href;
  const loading = pdfjs.getDocument({ data, cMapUrl: new URL("./vendor/pdfjs/cmaps/", import.meta.url).href,
    cMapPacked: true, standardFontDataUrl: new URL("./vendor/pdfjs/standard_fonts/", import.meta.url).href,
    disableFontFace: true, useSystemFonts: false, useWasm: false, isEvalSupported: false, stopAtErrors: true });
  // No password prompt can remain pending while the queue is paused or reset.
  loading.onPassword = (updatePassword) => updatePassword(new Error("Password-protected PDF cannot be imported. Save an unlocked copy first."));
  const cancel = () => { void loading.destroy(); };
  signal?.addEventListener("abort", cancel, { once: true });
  let pdf;
  try {
    pdf = await loading.promise; check(signal);
    if (pdf.numPages > PDF_IMPORT_LIMITS.pages) throw new Error("PDF exceeds 10,000 pages. Split it before importing.");
    onLoad({ pages: pdf.numPages, fingerprint });
    for (let pageNumber = 1; pageNumber <= pdf.numPages; pageNumber += 1) {
      check(signal); const page = await pdf.getPage(pageNumber); check(signal);
      let content;
      try { content = await page.getTextContent(); } finally { page.cleanup(); }
      check(signal);
      const lines = []; let bytes = 0;
      for (const item of content.items) {
        if (typeof item.str !== "string") continue;
        const text = item.str.replaceAll("\0", "") + (item.hasEOL ? "\n" : " ");
        bytes += encoder.encode(text).length;
        if (bytes > PDF_IMPORT_LIMITS.pageTextBytes) throw new Error(`Page ${pageNumber} exceeds the 16 MiB extracted-text limit. Split this page before importing.`);
        lines.push(text);
      }
      const text = lines.join("").trim();
      yield { page: pageNumber, pages: pdf.numPages, fingerprint, parts: text ? splitPdfText(text) : [] };
    }
  } catch (error) {
    if (signal?.aborted) throw aborted();
    if (/password|destroyed/i.test(error.message || "")) throw new Error("Password-protected PDF cannot be imported. Save an unlocked copy first.");
    throw error;
  } finally {
    signal?.removeEventListener("abort", cancel);
    await loading.destroy();
  }
}
