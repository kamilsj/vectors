const { defineConfig } = require("@playwright/test");
const existingServer = process.env.VECTORS_WEB_BASE_URL;

module.exports = defineConfig({
  testDir: "./tests",
  testMatch: "web-*.spec.cjs",
  fullyParallel: true,
  workers: process.env.CI ? 2 : 4,
  use: {
    baseURL: existingServer || "http://127.0.0.1:4173",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    launchOptions: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE
      ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE }
      : {},
  },
  webServer: existingServer ? undefined : {
    command: "node tests/web-server.cjs",
    url: "http://127.0.0.1:4173",
    reuseExistingServer: !process.env.CI,
    timeout: 10_000,
  },
});
