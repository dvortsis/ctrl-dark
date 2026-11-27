const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: './tests/ui',
  fullyParallel: true,
  workers: 2,
  use: {
    browserName: 'chromium',
    channel: process.env.CI ? undefined : 'msedge',
    headless: true,
    trace: 'retain-on-failure',
  },
});
