/// <reference types="vitest/config" />
import { execFileSync } from 'node:child_process'
import { readFileSync, readdirSync } from 'node:fs'
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
  plugins: [preact(), thirdPartyLicenses()],
  define: {
    __AXON_WEB_VERSION__: JSON.stringify(webClientVersion()),
    __AXON_WEB_BUILT_AT__: JSON.stringify(new Date().toISOString()),
  },
  server: {
    allowedHosts,
    proxy: axonProxy,
  },
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
