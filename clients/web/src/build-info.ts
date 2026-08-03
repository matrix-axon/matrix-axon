export interface BuildInfo {
  /** Human-readable release, from `package.json` (e.g. `0.1.0`). */
  release: string
  /** Exact build id — the git short hash, or a `VITE_AXON_WEB_VERSION` label. */
  version: string
  builtAt: string
  builtAtLabel: string
  /** `release+version`, the one string to quote in a bug report. */
  displayVersion: string
}

function formatBuiltAt(iso: string): string {
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) {
    return iso
  }
  return date.toLocaleString(undefined, {
    dateStyle: 'medium',
    timeStyle: 'short',
  })
}

/**
 * Semver build metadata: `0.1.0+ab12cd34ef56`. The release alone can't identify
 * a build — every test build of a release shares it — so the hash always rides
 * along. A build with no usable hash (`unknown`, from a build without git)
 * degrades to the bare release rather than printing `0.1.0+unknown`.
 */
export function formatVersion(release: string, version: string): string {
  // `-dirty` is appended to whatever the hash lookup produced, so a gitless
  // build in a dirty tree reports `unknown-dirty` — as anonymous as `unknown`,
  // and worth no more room in the footer.
  const hash = version.replace(/-dirty$/, '')
  if (hash === '' || hash === 'unknown') {
    return release
  }
  return `${release}+${version}`
}

export const BUILD_INFO: BuildInfo = {
  release: __AXON_WEB_RELEASE__,
  version: __AXON_WEB_VERSION__,
  builtAt: __AXON_WEB_BUILT_AT__,
  builtAtLabel: formatBuiltAt(__AXON_WEB_BUILT_AT__),
  displayVersion: formatVersion(__AXON_WEB_RELEASE__, __AXON_WEB_VERSION__),
}
