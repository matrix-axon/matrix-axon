import { defineConfig } from '@playwright/test'

/**
 * A runner configuration for `flaky-policy-reporter.vitest.ts`, never for the
 * lanes. The scenarios do not touch a page, so no browser launches and no mock
 * server is needed. The projects only name a browser, because that is what the
 * reporter's WebKit scoping reads.
 */
export default defineConfig({
  testDir: '.',
  testMatch: 'scenarios.ts',
  retries: 1,
  workers: 1,
  // `list` alongside, so each case can also check what the runner itself
  // counted. Otherwise a pattern that matched nothing would exit 1, too.
  reporter: [['list'], ['../flaky-policy-reporter.ts']],
  projects: [
    { name: 'chromium', use: { browserName: 'chromium' } },
    { name: 'webkit-desktop', use: { browserName: 'webkit' } },
  ],
})
