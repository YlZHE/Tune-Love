import { defineConfig } from "@playwright/test";
export default defineConfig({
  testDir: "./tests",
  outputDir: "artifacts/browser-tests",
  use: { channel: "msedge", headless: true, viewport: { width: 468, height: 242 }, baseURL: "http://127.0.0.1:1420" },
  webServer: { command: "npm run dev", url: "http://127.0.0.1:1420", reuseExistingServer: true },
});
