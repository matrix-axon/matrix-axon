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

/**
 * The address a *device* must reach this dev server on, set by the Tauri CLI
 * for `tauri ios dev` and `tauri android dev`.
 *
 * On a phone, `localhost` is the phone. Vite binds loopback by default, so
 * without this the CLI sits repeating "Waiting for your frontend dev server to
 * start on http://<lan-ip>:5173/" until it is killed — nothing is listening
 * there and nothing ever will be.
 *
 * Absent for browser and desktop development, which keeps the dev server on
 * loopback: one reachable across the LAN is one every device on the LAN can
 * read, including whatever session it is signed into.
 *
 * An empty value is treated as absent, and that is load-bearing rather than
 * tidiness. `'' ?? false` is `''`, which Vite reads as "bind every interface"
 * — so exporting `TAURI_DEV_HOST=` with no value, the ordinary way to clear a
 * variable in a shell, would silently publish the dev server to the whole
 * network. Measured: it listened on `*:5205`.
 */
const tauriDevHost = process.env.TAURI_DEV_HOST?.trim() || undefined
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

/**
 * Exit when whatever started this dev server goes away.
 *
 * `tauri ios dev` runs the dev server as its `beforeDevCommand` child and
 * reaps it on a clean exit — but not when it dies abnormally, and an
 * interrupted mobile build dies abnormally often. The dev server survives,
 * keeps 5173 and 1421, and `strictPort` below then fails the *next* run.
 *
 * That failure is close to unreadable. The CLI has already started xcodebuild
 * by the time the dev server gives up, and xcodebuild's Rust phase calls back
 * to the now-dead CLI over a WebSocket, so the honest "Port 5173 is already in
 * use" scrolls past six hundred lines of build settings and the run ends in
 * `panicked at mobile/mod.rs:403 ... ConnectionRefused` instead. Measured: it
 * cost three consecutive runs to three separate orphans.
 *
 * Watching our own parent is not enough, and this is the whole subtlety: the
 * chain is `tauri ios dev` -> `pnpm dev` -> `vite`, and it is the *middle* one
 * that gets orphaned. Killing the CLI leaves `pnpm` reparented to pid 1 with
 * our own `process.ppid` pointing at it, unchanged. So record every ancestor
 * at startup and watch for any of them disappearing.
 *
 * A server started detached is left alone, and "detached" has to mean more
 * than having no parent at startup. `nohup pnpm dev &` from a script leaves
 * that script an ancestor for as long as it runs, so watching for any ancestor
 * to disappear is exactly how such a server gets killed a second after its
 * launcher exits — the one case the exemption is supposed to cover.
 *
 * So the supervision has to be armed, not inferred. `TAURI_ENV_PLATFORM` is
 * set by the Tauri CLI for its `before*Command` hooks and by nothing else —
 * it is named in the CLI's own config schema for `beforeDevCommand`, next to
 * `TAURI_ENV_ARCH`, `TAURI_ENV_FAMILY`, `TAURI_ENV_PLATFORM_VERSION`,
 * `TAURI_ENV_PLATFORM_TYPE` and `TAURI_ENV_DEBUG` (read out of
 * `@tauri-apps/cli` 2.11.4's binary). A plain `pnpm dev`, a `nohup pnpm dev >
 * log &`, and every vitest run never see it, so they are outside this
 * entirely.
 *
 * An earlier version tested `process.stdout.isTTY` instead, reasoning that a
 * `beforeDevCommand` child writes to a pipe while a detached one writes to a
 * file or /dev/null. That is wrong in both directions. A file and /dev/null
 * are not ttys either, so `nohup pnpm dev > log &` — and an interactive shell
 * that later closes — was supervised and shut itself down about a second
 * after its launcher exited, which is the one case the exemption exists for.
 * And it assumed the CLI never hands its child a terminal, which this config
 * has no way to know; if it does, the test disarms the plugin exactly where
 * it is needed. A variable the CLI documents is a fact. The shape of fd 1 is
 * a guess.
 */
function exitWhenOrphaned(): Plugin {
  return {
    name: 'axon-exit-when-orphaned',
    apply: 'serve',
    configureServer(server) {
      if (process.env.TAURI_ENV_PLATFORM === undefined) {
        return
      }
      const ancestors = ancestorPids()
      if (ancestors.length === 0) {
        return
      }
      const poll = setInterval(() => {
        const gone = ancestors.find((pid) => !isAlive(pid))
        if (gone === undefined) {
          return
        }
        clearInterval(poll)
        server.config.logger.warn(
          `[axon] pid ${gone}, which this dev server was started under, has ` +
            'exited; shutting down rather than holding its ports.',
        )
        void server.close().then(() => process.exit(0))
      }, 1000)
      // Never a reason to keep the process alive on its own account.
      poll.unref()
    },
  }
}

/**
 * Our parent, its parent, and so on — pid 1 excluded, since init outlives us
 * by definition.
 *
 * Empty when there is nothing to watch: on Windows, which has no `ps` and no
 * reparenting to observe, and for a server whose parent is already pid 1.
 *
 * Whether to watch at all is the caller's decision, not this one's — see
 * `exitWhenOrphaned`.
 */
function ancestorPids(): number[] {
  if (process.platform === 'win32') {
    return []
  }
  const pids: number[] = []
  let pid = process.ppid
  // `ps` walking is bounded by the depth of the process tree; the cap is only
  // here so a cycle from a recycled pid cannot spin forever.
  while (pid > 1 && pids.length < 32) {
    pids.push(pid)
    try {
      const parent = execFileSync('ps', ['-o', 'ppid=', '-p', String(pid)], {
        encoding: 'utf8',
      }).trim()
      pid = Number.parseInt(parent, 10)
    } catch {
      // The process exited while we walked, or `ps` is not where we expect.
      break
    }
    if (!Number.isInteger(pid)) {
      break
    }
  }
  return pids
}

/** Signal 0 tests for existence without delivering anything. */
function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0)
    return true
  } catch (error) {
    // EPERM means it exists and is not ours to signal, which is still alive.
    return (error as NodeJS.ErrnoException).code === 'EPERM'
  }
}

export default defineConfig({
  plugins: [
    preact(),
    thirdPartyLicenses(),
    versionManifestPlugin(),
    exitWhenOrphaned(),
  ],
  define: {
    __AXON_WEB_RELEASE__: JSON.stringify(RELEASE),
    __AXON_WEB_VERSION__: JSON.stringify(VERSION),
    __AXON_WEB_BUILT_AT__: JSON.stringify(BUILT_AT),
  },
  server: {
    // `false` rather than undefined: that is Vite's "loopback only", and it is
    // what every non-mobile run should get.
    host: tauriDevHost ?? false,
    // `tauri.conf.json`'s `devUrl` names port 5173, so Vite quietly moving to
    // 5174 because something already holds 5173 produces a dev server Tauri
    // never finds — an indefinite wait from a cause that looks nothing like
    // it. That is what `beforeDevCommand`'s `pnpm dev --strictPort` is for, and
    // it already covered the Tauri path before this file mentioned ports.
    //
    // Deliberately not `strictPort` here. This block is shared with ordinary
    // browser development, where the port-bump fallback is the right
    // behaviour: a second `pnpm dev` from a jj workspace should take 5174, not
    // refuse to start because the main checkout holds 5173.
    port: 5173,
    // The HMR socket has to point back at this machine. Left to infer, it
    // resolves against the page's own origin, which on a device is the device.
    hmr: tauriDevHost
      ? { protocol: 'ws', host: tauriDevHost, port: 1421 }
      : undefined,
    // Vite rejects requests whose Host header it does not recognise. It admits
    // bare IP addresses, so this is belt and braces for the case where the CLI
    // hands over a hostname instead.
    allowedHosts: tauriDevHost ? [...allowedHosts, tauriDevHost] : allowedHosts,
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
