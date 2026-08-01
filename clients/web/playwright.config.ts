import { defineConfig, devices } from '@playwright/test'

const PORT = 4599
const BASE_URL = `http://127.0.0.1:${PORT}`

/**
 * The web e2e lanes: a headless Chromium drives the built app against the
 * dependency-free mock backend (`e2e/mock-server.mjs`), which serves `dist/`
 * and a real `/v1/ws`. Run `pnpm build` first — the mock serves static assets,
 * so the app must be built. `reuseExistingServer` keeps local reruns fast; CI
 * starts fresh.
 *
 * Lanes: live sockets (ADR 0061), two-pane layout (ADR 0062), keyboard
 * shortcuts (ADR 0078).
 *
 * One worker, because the mock backend is a single process with shared state:
 * `two-tabs-live` posts `/__e2e/drop-sockets`, which severs *every* connected
 * socket. Run in parallel, that would knock the other specs offline mid-test.
 */
export default defineConfig({
  testDir: './e2e',
  // `e2e/demo/` is the ADR 0086 recording lane, not a test lane: it needs a
  // real seeded axon and a manifest path, so collecting it here would fail the
  // PR gate on every run. It has its own config (`playwright.demo.config.ts`).
  testIgnore: '**/demo/**',
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? 'github' : 'list',
  use: {
    baseURL: BASE_URL,
    trace: 'on-first-retry',
  },
  projects: [
    { name: 'chromium', use: { ...devices['Desktop Chrome'] } },
    // The perf lane (ADR 0071) also runs under WebKit — Apple's engine, what
    // iOS Safari uses — to catch engine-specific costs a Chromium run cannot,
    // like a constant transition cost that reproduces regardless of data size.
    // Gated on PERF so the default e2e sweep neither launches WebKit nor runs
    // twice, and scoped to the one perf spec. WebKit rejects `isMobile`, so the
    // iPhone 13 profile is built without it (viewport + UA + touch are what the
    // single-pane layout and mobile chrome key off).
    ...(process.env.PERF
      ? [
          {
            name: 'webkit' as const,
            testMatch: /timeline-back-perf\.spec\.ts/,
            use: {
              browserName: 'webkit' as const,
              viewport: devices['iPhone 13'].viewport,
              userAgent: devices['iPhone 13'].userAgent,
              deviceScaleFactor: devices['iPhone 13'].deviceScaleFactor,
              hasTouch: true,
            },
          },
        ]
      : []),
  ],
  webServer: {
    command: 'node e2e/mock-server.mjs',
    url: BASE_URL,
    env: { PORT: String(PORT) },
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
  },
})
