// Pure parsing/selection logic for the third-party open-source disclosure,
// split out from vite.config.ts so it can be unit-tested without shelling out
// to pnpm or touching the filesystem. The node-specific glue (execFileSync,
// readdirSync/readFileSync, memoization, the VITEST short-circuit) stays in
// vite.config.ts and injects its side effects into `collectDisclosure`.
//
// This module has no `node:` imports on purpose: it stays hermetic and the app
// bundle never pulls it in (only the generated virtual module is imported at
// runtime).

export interface ThirdPartyLicense {
  name: string
  version: string
  license: string
  text: string
  homepage?: string
  author?: string
}

// `pnpm licenses list --prod --json` groups packages by SPDX license id; each
// entry may cover several installed versions/paths of the same package name.
export interface PnpmLicenseEntry {
  name: string
  versions?: string[]
  paths?: string[]
  license?: string
  homepage?: string
  author?: string
}

// Also matches suffixed variants like LICENSE-MIT / LICENSE.APACHE / COPYING.LESSER
// that dual-licensed packages ship, not just the bare names.
const LICENSE_FILE_RE = /^(licen[cs]e|copying|notice)([-_.].*)?$/i

// Rank candidate license filenames so the same package yields the same text on
// every machine regardless of `readdir` order — filesystems don't guarantee
// alphabetical order, and dual-licensed packages ship several files. Prefer the
// LICENSE family over COPYING over NOTICE, and within a family a bare name over
// a doc-extensioned one (LICENSE.md) over a suffixed variant (LICENSE-MIT).
function licenseFileRank(name: string): number {
  const lower = name.toLowerCase()
  const family = /^licen[cs]e/.test(lower) ? 0 : /^copying/.test(lower) ? 1 : 2
  const variant = /^(licen[cs]e|copying|notice)$/.test(lower)
    ? 0
    : /^(licen[cs]e|copying|notice)\.(md|txt|rst)$/.test(lower)
      ? 1
      : 2
  return family * 10 + variant
}

/**
 * Deterministically choose the license file to disclose from a directory
 * listing, or null when none match. Order-independent: ranked by preference,
 * then alphabetically as a stable tie-break.
 */
export function pickLicenseFile(entries: string[]): string | null {
  const matches = entries.filter((name) => LICENSE_FILE_RE.test(name))
  if (matches.length === 0) {
    return null
  }
  matches.sort((a, b) => {
    const rankDelta = licenseFileRank(a) - licenseFileRank(b)
    return rankDelta !== 0
      ? rankDelta
      : a.localeCompare(b, 'en', { sensitivity: 'base' })
  })
  return matches[0]
}

/**
 * Turn raw `pnpm licenses list --prod --json` stdout into a sorted, de-duplicated
 * disclosure. `readLicense` resolves a package directory to its license text (or
 * null); a package with no readable license file gets a declared-license note.
 *
 * Throws if `raw` is not the expected JSON object — callers are expected to
 * treat that as "disclosure unavailable" (see `collectDisclosure`).
 */
export function buildDisclosure(
  raw: string,
  readLicense: (dir: string) => string | null,
): ThirdPartyLicense[] {
  const grouped = JSON.parse(raw) as unknown
  if (grouped === null || typeof grouped !== 'object') {
    throw new Error('expected a JSON object keyed by license id')
  }

  const byKey = new Map<string, ThirdPartyLicense>()
  for (const entries of Object.values(
    grouped as Record<string, PnpmLicenseEntry[]>,
  )) {
    for (const entry of entries) {
      const version = entry.versions?.[0] ?? '0.0.0'
      const key = `${entry.name}@${version}`
      if (byKey.has(key)) {
        continue
      }
      const dir = entry.paths?.[0]
      const text =
        (dir ? readLicense(dir) : null) ??
        `No license file was bundled with this package. Declared license: ${entry.license ?? 'UNKNOWN'}.`
      byKey.set(key, {
        name: entry.name,
        version,
        license: entry.license ?? 'UNKNOWN',
        text,
        homepage: entry.homepage,
        author: entry.author,
      })
    }
  }

  return [...byKey.values()].sort((a, b) =>
    a.name.localeCompare(b.name, 'en', { sensitivity: 'base' }),
  )
}

/**
 * Orchestrate disclosure generation with all side effects injected, so the
 * failure paths are testable. Any failure — the pnpm call *or* parsing its
 * output — degrades to an empty list with a warning rather than failing the
 * build.
 */
export function collectDisclosure(
  runPnpm: () => string,
  readLicense: (dir: string) => string | null,
  warn: (message: string, error: unknown) => void,
): ThirdPartyLicense[] {
  let raw: string
  try {
    raw = runPnpm()
  } catch (error) {
    warn(
      '[thirdparty] `pnpm licenses list` failed; disclosure will be empty:',
      error,
    )
    return []
  }
  try {
    return buildDisclosure(raw, readLicense)
  } catch (error) {
    warn(
      '[thirdparty] could not parse `pnpm licenses list` output; disclosure will be empty:',
      error,
    )
    return []
  }
}

/**
 * The command that runs this build's own pnpm.
 *
 * A bare `execFileSync('pnpm')` cannot work on Windows: `CreateProcess` finds
 * only `.exe` files, and a pnpm installed by `pnpm/action-setup` or npm is a
 * `pnpm.cmd` shim, so the spawn failed with ENOENT and the disclosure shipped
 * empty. pnpm sets `npm_execpath` for the scripts it runs, naming the exact
 * entry point that is running this build; a JS entry point is run with the
 * current Node, which needs no shell and no PATH lookup. Outside pnpm
 * (`execPath` unset, or naming npm or yarn) it falls back to `pnpm`.
 */
export function pnpmCommand(
  execPath: string | undefined,
  nodePath: string,
): { command: string; args: string[] } {
  const name = (execPath?.split(/[\\/]/).pop() ?? '').toLowerCase()
  if (execPath && name.startsWith('pnpm')) {
    if (/\.[cm]?js$/.test(name)) {
      return { command: nodePath, args: [execPath] }
    }
    // A `.cmd`/`.bat`/`.ps1` shim is what fails to spawn; only a binary (or
    // an extensionless executable) can be run directly.
    if (!/\.(cmd|bat|ps1)$/.test(name)) {
      return { command: execPath, args: [] }
    }
  }
  return { command: 'pnpm', args: [] }
}
