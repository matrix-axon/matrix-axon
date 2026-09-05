/// <reference types="vitest/config" />
import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { defineConfig, type Plugin } from 'vite'
import preact from '@preact/preset-vite'
import {
  collectDisclosure,
  pickLicenseFile,
  type ThirdPartyLicense,
} from './src/thirdparty-disclosure.ts'

// In development the axon server runs on another origin and serves no CORS
// headers (ADR 0046), so the dev server proxies API and WebSocket traffic.
// Override the target with AXON_SERVER_URL if your server is not on :8080.
const axonServer = process.env.AXON_SERVER_URL ?? 'http://localhost:8080'

// The same proxy is given to `vite preview`, so the *built* bundle can be
// pointed at a real axon without standing up a reverse proxy of its own. The
// demo recording lane (ADR 0086 phase 3) is why that matters: a demo should
// show the production bundle, and the axon it reads is a throwaway `--corpus`
// local stack that lands on a different port every run.
const axonProxy = {
  '/v1': {
    target: axonServer,
    changeOrigin: true,
    ws: true,
  },
}

// Vite blocks dev-server requests whose Host header isn't localhost. To reach
// the dev server through another hostname (a tunnel, a LAN name, a reverse
// proxy), list the extra hostnames — comma-separated — without editing this
// file: AXON_DEV_ALLOWED_HOSTS=axon-web.example.net,axon-dev.local pnpm dev
const allowedHosts = (process.env.AXON_DEV_ALLOWED_HOSTS ?? '')
  .split(',')
  .map((host) => host.trim())
  .filter((host) => host !== '')
const webClientDir = fileURLToPath(new URL('.', import.meta.url))

function git(args: string[]): string | null {
  try {
    return execFileSync('git', args, {
      cwd: webClientDir,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim()
  } catch {
    return null
  }
}

function webClientVersion(): string {
  const override = process.env.VITE_AXON_WEB_VERSION?.trim()
  if (override) {
    return override
  }

  const hash = git(['rev-parse', '--short=12', 'HEAD']) ?? 'unknown'
  const dirty = git(['status', '--short', '--', '.']) !== ''
  return dirty ? `${hash}-dirty` : hash
}

/**
 * The human-readable release, from this package's `version`. The git hash below
 * is the exact build id and the only thing the update check compares; this is
 * the number a bug report can name. Read with `readFileSync` rather than a JSON
 * import so the config keeps working under every loader that runs it (build,
 * dev, preview, vitest), and because `deploy/web/Dockerfile` already copies
 * `package.json` into its build stage — no new build arg.
 */
const RELEASE = String(
  JSON.parse(readFileSync(join(webClientDir, 'package.json'), 'utf8')).version,
)

/**
 * Stamped once, at module scope, so `define` below and the emitted
 * `version.json` can never disagree. They must be byte-identical: the running
 * client compares its baked-in `__AXON_WEB_VERSION__` against the fetched
 * manifest, and any difference reads as "a new build is available".
 */
const VERSION = webClientVersion()
const BUILT_AT = new Date().toISOString()

/** The `version.json` body — the origin's answer to "what build do you serve?" */
function versionManifest(): string {
  return `${JSON.stringify({ release: RELEASE, version: VERSION, builtAt: BUILT_AT }, null, 2)}\n`
}

/**
 * Build identity as a fetchable file, plus the preview server's missing-asset
 * guard. Together these are what let a running client notice a new deploy and
 * reload itself instead of hanging (ADR 0087).
 *
 * The manifest is emitted by `generateBundle` — i.e. only by a real build — and
 * in preview it is served out of `dist/` like any other asset. That is
 * deliberate: `vite preview` re-evaluates this config, so synthesizing the
 * manifest at preview time would stamp a *fresh* `BUILT_AT` (and, on a dirty
 * tree, a different hash) that disagrees with the values already baked into the
 * built bundle. The client would see a permanent mismatch and reload forever.
 * Only the dev server, which has no `dist/` to serve, may synthesize it.
 */
function versionManifestPlugin(): Plugin {
  let distDir = join(webClientDir, 'dist')
  return {
    name: 'axon-version-manifest',
    configResolved(config) {
      distDir = config.build.outDir.startsWith('/')
        ? config.build.outDir
        : join(config.root, config.build.outDir)
    },
    generateBundle() {
      this.emitFile({
        type: 'asset',
        fileName: 'version.json',
        source: versionManifest(),
      })
    },
    configureServer(server) {
      server.middlewares.use('/version.json', (_req, res) => {
        res.setHeader('Content-Type', 'application/json')
        res.setHeader('Cache-Control', 'no-store')
        res.end(versionManifest())
      })
    },
    configurePreviewServer(server) {
      // Registered in the hook *body*, not in the returned post-hook: vite
      // awaits `configurePreviewServer` before it installs its own static and
      // HTML-fallback middleware, whereas the returned function runs after
      // both. Only the former can intercept a request ahead of the fallback.
      //
      // Why intercept at all: vite's `htmlFallbackMiddleware` rewrites any
      // unmatched GET whose `Accept` includes `*/*` to `/index.html` — and
      // `*/*` is exactly what a `<script type="module">` and a dynamic
      // `import()` send. So a hashed chunk that a redeploy has deleted comes
      // back as `200 text/html` instead of `404`, the module fails to parse,
      // and a client still running the previous build hangs. Assets are
      // content-hashed, so a miss under /assets/ is never a route — 404 it.
      server.middlewares.use((req, res, next) => {
        const url = req.url ?? '/'
        if (!url.startsWith('/assets/')) {
          return next()
        }
        let pathname: string
        try {
          pathname = decodeURIComponent(
            new URL(url, 'http://localhost').pathname,
          )
        } catch {
          return next()
        }
        // `join` normalizes `..`; confirm we stayed inside dist before answering.
        const file = join(distDir, pathname)
        if (file.startsWith(join(distDir, 'assets')) && existsSync(file)) {
          return next()
        }
        res.statusCode = 404
        res.setHeader('Content-Type', 'text/plain')
        res.end('not found\n')
      })
    },
  }
}

// Third-party open-source disclosure, generated at build time from the pnpm
// production dependency tree (ADR-style parity with the Rust THIRDPARTY.md).
// This runs during `vite build`/`vite dev` with only pnpm + node_modules on
// hand (no git, no cargo), so it works inside deploy/web/Dockerfile too. The
// pure parsing/selection logic lives in src/thirdparty-disclosure.ts; here we
// only provide the node-specific side effects.
function readLicenseText(dir: string): string | null {
  let entries: string[]
  try {
    entries = readdirSync(dir)
  } catch {
    return null
  }
  const match = pickLicenseFile(entries)
  if (!match) {
    return null
  }
  try {
    return readFileSync(join(dir, match), 'utf8').trim()
  } catch {
    return null
  }
}

let thirdPartyCache: ThirdPartyLicense[] | null = null

function collectThirdPartyLicenses(): ThirdPartyLicense[] {
  if (thirdPartyCache) {
    return thirdPartyCache
  }
  // Tests never need the real disclosure; skip the pnpm shell-out so vitest
  // stays fast and hermetic.
  if (process.env.VITEST) {
    thirdPartyCache = []
    return thirdPartyCache
  }

  thirdPartyCache = collectDisclosure(
    () =>
      execFileSync('pnpm', ['licenses', 'list', '--prod', '--json'], {
        cwd: webClientDir,
        encoding: 'utf8',
        stdio: ['ignore', 'pipe', 'ignore'],
        maxBuffer: 64 * 1024 * 1024,
      }),
    readLicenseText,
    (message, error) =>
      console.warn(message, error instanceof Error ? error.message : error),
  )
  return thirdPartyCache
}

const VIRTUAL_LICENSES_ID = 'virtual:thirdparty-licenses'

function thirdPartyLicenses(): Plugin {
  const resolvedId = `\0${VIRTUAL_LICENSES_ID}`
  return {
    name: 'axon-thirdparty-licenses',
    resolveId(id) {
      return id === VIRTUAL_LICENSES_ID ? resolvedId : null
    },
    load(id) {
      if (id !== resolvedId) {
        return null
      }
      return `export default ${JSON.stringify(collectThirdPartyLicenses())}`
    },
  }
}

export default defineConfig({
  plugins: [preact(), thirdPartyLicenses(), versionManifestPlugin()],
  define: {
    __AXON_WEB_RELEASE__: JSON.stringify(RELEASE),
    __AXON_WEB_VERSION__: JSON.stringify(VERSION),
    __AXON_WEB_BUILT_AT__: JSON.stringify(BUILT_AT),
  },
  server: {
    allowedHosts,
    proxy: axonProxy,
    watch: {
      // `src-tauri/` is a Rust crate (ADR 0102, M-W12) that lives inside this
      // package, so Vite's watcher walks into it by default — including
      // `target/`, which is tens of thousands of build artifacts and gigabytes
      // of them. Nothing under here is a frontend source, and the Tauri CLI
      // watches the Rust side itself.
      //
      // On Linux that is merely wasteful. On Windows a running executable is
      // locked, so `fs.watch` on `target/debug/deps/axon_shell.exe` throws
      // EBUSY and takes the whole dev server down — intermittently, because it
      // depends on whether a previous build's binary is still running when the
      // watcher reaches that file. `pnpm tauri dev` then dies with
      // "beforeDevCommand terminated with a non-zero status code", pointing at
      // nothing that looks like a cause.
      ignored: ['**/src-tauri/**'],
    },
  },
  // Besides the demo lane the proxy above was written for, `preview` is how
  // startup is *measured*: `pnpm dev` serves hundreds of unbundled ES modules,
  // which over a LAN to a phone inflates startup far beyond anything a user
  // would see, so any number covering bundle boot — ADR 0085's
  // `boot:room-list` summary especially — is meaningless taken in dev. Measure
  // against `pnpm build && pnpm preview --host`.
  preview: {
    allowedHosts,
    proxy: axonProxy,
  },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
    // Unit tests live in src; the Playwright e2e specs in e2e/ are run by
    // `pnpm test:e2e`, not vitest (both use the `.spec.ts` suffix).
    include: ['src/**/*.{test,spec}.{ts,tsx}'],
  },
})
