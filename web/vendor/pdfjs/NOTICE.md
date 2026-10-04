# PDF.js browser assets

PDF.js version **6.3.289**, pinned as `pdfjs-dist` in package.json/package-lock.json.
Upstream: https://github.com/mozilla/pdf.js
Browser integration: https://mozilla.github.io/pdf.js/examples/

`pdf.mjs` and `pdf.worker.mjs` are unmodified copies of `build/pdf.min.mjs`
and `build/pdf.worker.min.mjs`. The `cmaps` and `standard_fonts` directories
are copied unchanged from the same package. Their included license files
cover bundled character maps and fonts. PDF.js is Apache-2.0; see LICENSE.
These assets are served locally by vectors-server. No CDN is used.

Refresh only from the pinned dependency and review its license notices:

    cp node_modules/pdfjs-dist/build/pdf.min.mjs web/vendor/pdfjs/pdf.mjs
    cp node_modules/pdfjs-dist/build/pdf.worker.min.mjs web/vendor/pdfjs/pdf.worker.mjs
    cp -R node_modules/pdfjs-dist/cmaps node_modules/pdfjs-dist/standard_fonts web/vendor/pdfjs/
    cp node_modules/pdfjs-dist/LICENSE web/vendor/pdfjs/LICENSE
