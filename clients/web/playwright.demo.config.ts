import { readFileSync } from 'node:fs'
import { defineConfig, devices } from '@playwright/test'

/**
 * The demo recording lane (ADR 0086 phase 3).
 *
 * This is **not** a test lane. It drives the built app through a scripted tour
 * of the demo corpus and keeps the video Playwright records of each scene. It
 * lives in its own config, rather than as extra projects in
 * `playwright.config.ts`, for one blunt reason: the PR gate runs
 * `playwright test` with no arguments, and everything about this lane —
 * the backend, the server command, the video capture, the pacing — is
 * different. `pnpm test:e2e` never sees it.
 *
 * Unlike the e2e lane, the backend here is a **real axon**: a throwaway
 * Synapse + Postgres + axon stack seeded from
 * `smoke/local-stack/corpus/demo.toml`. That is the whole point of ADR 0086 —
 * search results come from a real Tantivy index, media really traverses the
 * media proxy, and the WebSocket is the real one. `e2e/mock-server.mjs` is
 * left alone, serving the lane it was built for.
 *
 * ```sh
 * # 1. bring a demo world up and leave it running (from the repo root)
 * cargo run -p axon-smoke-local-stack -- up \
 *     --manifest /tmp/demo.json \
 *     --corpus smoke/local-stack/corpus/demo.toml --keep-up
 *
 * # 2. record (from clients/web)
 * DEMO_MANIFEST=/tmp/demo.json pnpm demo
 * ```
 *
 * See `README.md` § Demo recording for what to do with the output.
 */

const PORT = Number(process.env.DEMO_PORT ?? 4600)
const BASE_URL = `http://127.0.0.1:${PORT}`

/**
 * Read the manifest here, at config load, so a missing or corpus-less stack
 * fails with a sentence a human can act on rather than as a pile of timed-out
 * scenes twenty minutes into a recording session.
 */
function axonBaseUrl(): string {
  const path = process.env.DEMO_MANIFEST
  if (path === undefined || path === '') {
    throw new Error(
      'DEMO_MANIFEST is required: the path to the manifest of a local stack ' +
        'brought up with --corpus. See playwright.demo.config.ts for the ' +
        'two commands.',
    )
  }
  const manifest: unknown = JSON.parse(readFileSync(path, 'utf8'))
  if (typeof manifest !== 'object' || manifest === null) {
    throw new Error(`${path} is not a local-stack manifest`)
  }
  const { axon_base_url: url, demo } = manifest as Record<string, unknown>
  if (demo === undefined || demo === null) {
    throw new Error(
      `${path} has no "demo" section: bring the stack up with ` +
        '--corpus smoke/local-stack/corpus/demo.toml',
    )
  }
  if (typeof url !== 'string') {
    throw new Error(`${path} has no axon_base_url`)
  }
  return url
}

/**
 * Videos are captured at CSS-pixel size, so a phone viewport would otherwise
 * yield a 390px-wide recording. Scaling up resamples from the real
 * device-scale-factor-3 raster the page is already painted at, rather than
 * upscaling something small — every factor up to 3x is downsampling from a
 * sharper source, not interpolation.
 *
 * 2x rather than the full 3x because the ceiling here is file size, not
 * sharpness. 3x is 1170x2532, which is both larger than any browser will show
 * a portrait video inline on the docs page and roughly double the bytes; at 2x
 * the take is ~6 MB and still reads as crisp, since it is a downsample of a 3x
 * raster rather than a native 2x one. Revisit if the video ever needs to be
 * legible full-screen on a hidpi display.
 */
const iPhone = devices['iPhone 13']

export default defineConfig({
  testDir: './e2e/demo',
  // A recording is a sequence, not a suite: one worker, in file order, and a
  // failure stops the take rather than retrying into a different world state.
  workers: 1,
  fullyParallel: false,
  retries: 0,
  reporter: 'list',
  // Scenes dwell on purpose, and a real stack is slower than a mock.
  timeout: 180_000,
  outputDir: './demo-artifacts',
  use: {
    baseURL: BASE_URL,
    trace: 'off',
    screenshot: 'off',
    // The app's own `theme` setting is what actually decides (seeded in
    // `stage`), but pin the emulated media query to match so anything reading
    // `prefers-color-scheme` directly — form controls, scrollbars, the UA
    // `color-scheme` — agrees with it. A recording must not depend on the
    // preferences of the machine it was made on.
    colorScheme: 'light',
  },
  projects: [
    {
      name: 'demo-desktop',
      testMatch: /desktop\.spec\.ts/,
      use: {
        ...devices['Desktop Chrome'],
        viewport: { width: 1440, height: 900 },
        video: { mode: 'on', size: { width: 1440, height: 900 } },
      },
    },
    {
      // Mobile is not a narrow desktop window (ADR 0086): a device descriptor
      // is what earns real touch, device pixel ratio, and a phone-shaped
      // viewport, and the mobile layout collapses the room list and the spaces
      // rail behind navigation — so the scenes differ in kind, not in width.
      name: 'demo-mobile',
      testMatch: /mobile\.spec\.ts/,
      use: {
        ...iPhone,
        video: {
          mode: 'on',
          size: {
            width: iPhone.viewport.width * 2,
            height: iPhone.viewport.height * 2,
          },
        },
      },
    },
  ],
  webServer: {
    // The production bundle, not the dev server: `vite preview` carries the
    // same `/v1` proxy (vite.config.ts), which is what lets the page reach an
    // axon that serves no CORS headers.
    command: `pnpm build && pnpm preview --host 127.0.0.1 --port ${PORT} --strictPort`,
    url: BASE_URL,
    env: { AXON_SERVER_URL: axonBaseUrl() },
    // Deliberately never reused. The local stack binds a fresh axon port on
    // every `up`, and a preview server left over from the previous recording
    // proxies to the *previous* stack — which is dead, or worse, still alive
    // and serving a differently seeded world.
    reuseExistingServer: false,
    timeout: 300_000,
    stdout: 'ignore',
  },
})
