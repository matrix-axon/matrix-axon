import { signal, type ReadonlySignal } from '@preact/signals'
import { settleWithin } from '../settle-within'

/**
 * The origin's answer to "what build do you serve right now?" — `version.json`,
 * emitted by the `axon-version-manifest` plugin in `vite.config.ts` (ADR 0087).
 * `version` is the exact build id and the only field compared; `release` and
 * `builtAt` are for display.
 */
export interface VersionManifest {
  release: string
  version: string
  builtAt: string
}

/** Where the manifest lives, relative to the origin serving the SPA. */
export const VERSION_MANIFEST_PATH = '/version.json'

export type UpdateStatus =
  | 'idle'
  | 'checking'
  /** Checked, and the origin serves the build we are running. */
  | 'current'
  /** Checked, and the origin serves something else. */
  | 'available'
  /** The last check told us nothing (offline, 404, non-JSON body). */
  | 'error'

export interface UpdateChecker {
  /**
   * Whether the origin has a build other than ours. **Latches** — once true it
   * stays true. A check that transiently fails (a deploy restart drops the
   * connection mid-poll) must not retract a banner the user has already seen,
   * and the new build does not go away.
   */
  available: ReadonlySignal<boolean>
  /** The most recent manifest read, or `null` if none has parsed yet. */
  latest: ReadonlySignal<VersionManifest | null>
  /** Feedback for the Settings "Check for updates" button. */
  status: ReadonlySignal<UpdateStatus>
  /** Run one check now. Never rejects. */
  check(): Promise<void>
  /** Begin the periodic backstop poll. Idempotent. */
  start(): void
  /** Stop polling. Idempotent. */
  stop(): void
}

export interface UpdateCheckerOptions {
  /** The build this bundle is, i.e. `BUILD_INFO.version`. */
  currentVersion: string
  /** Reads the manifest; returns `null` when it learned nothing. */
  fetchManifest: () => Promise<VersionManifest | null>
  /** Backstop poll period. */
  intervalMs?: number
  /** Skip a scheduled poll while the document is hidden. */
  isVisible?: () => boolean
}

/**
 * Fifteen minutes. This is only a backstop: a deploy restarts the process that
 * proxies `/v1/ws`, so the socket drop reports a new build within seconds, and
 * returning to a backgrounded tab checks immediately. The interval exists for
 * the one case neither covers — a focused desktop tab whose socket happens to
 * stay up — so it is set for negligible cost, not for latency.
 */
export const DEFAULT_INTERVAL_MS = 15 * 60 * 1000

/**
 * How long a manifest read may take before it counts as having learned nothing.
 * Generous — this is a hang cutoff, not a latency target — but finite, because
 * an unbounded read wedges `check()` permanently (see `createUpdateChecker`).
 */
export const MANIFEST_TIMEOUT_MS = 10_000

/**
 * Backstop on a whole check, above `MANIFEST_TIMEOUT_MS` so it only fires for a
 * `fetchManifest` that does not bound itself. Frees the de-duplication slot; it
 * does not cancel the work, which settles into a `status` write later or never.
 */
const WATCHDOG_MS = MANIFEST_TIMEOUT_MS * 3

/**
 * A version string that identifies nothing. A build made without git reports
 * `unknown`, and every such build reports it identically — treating that as a
 * version would make two anonymous builds look equal and one anonymous build
 * look different from a hashed one, both wrongly.
 *
 * The `-dirty` suffix has to come off first. `webClientVersion()` appends it to
 * whatever the hash lookup produced, so a gitless build in a dirty tree reports
 * `unknown-dirty`, which is just as anonymous as `unknown` but was slipping
 * through as a real id — precisely the comparison this guards against.
 */
function identifiesABuild(version: unknown): version is string {
  if (typeof version !== 'string') {
    return false
  }
  const hash = version.replace(/-dirty$/, '')
  return hash !== '' && hash !== 'unknown'
}

/** Narrow a parsed body to a manifest, or `null` if it is not one. */
export function parseVersionManifest(value: unknown): VersionManifest | null {
  if (typeof value !== 'object' || value === null) {
    return null
  }
  const record = value as Record<string, unknown>
  if (!identifiesABuild(record.version)) {
    return null
  }
  return {
    release: typeof record.release === 'string' ? record.release : '',
    version: record.version,
    builtAt: typeof record.builtAt === 'string' ? record.builtAt : '',
  }
}

/**
 * Read `version.json` from the origin. Returns `null` for every failure —
 * offline, a 404, a body that is not the manifest — because "I could not find
 * out" must never be mistaken for "the build changed".
 *
 * Two defenses against the SPA fallback, which is the specific way this lies.
 * An SPA server asked for a path it does not have will happily return
 * `index.html` with `200 text/html` — vite's preview server does this for any
 * request whose `Accept` is the wildcard that `fetch` sends by default. So:
 * ask for JSON explicitly, which sidesteps that fallback, and then refuse
 * anything that did not come back as JSON, which covers the servers that
 * ignore `Accept` and fall back regardless.
 *
 * Bounded by `timeoutMs`, and the bound covers the **body read** as well as the
 * connection: a socket that opens and then stalls mid-body hangs
 * `response.json()` exactly as effectively as one that never answers. An
 * unbounded read here does not merely lose one check — it wedges the caller's
 * de-duplication forever, which is the whole of update detection.
 */
export async function fetchVersionManifest(
  path: string = VERSION_MANIFEST_PATH,
  fetchImpl: typeof fetch = fetch,
  timeoutMs: number = MANIFEST_TIMEOUT_MS,
): Promise<VersionManifest | null> {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), timeoutMs)
  try {
    const response = await fetchImpl(path, {
      cache: 'no-store',
      headers: { accept: 'application/json' },
      signal: controller.signal,
    })
    if (!response.ok) {
      return null
    }
    const contentType = response.headers.get('content-type') ?? ''
    if (!contentType.toLowerCase().includes('application/json')) {
      return null
    }
    return parseVersionManifest(await response.json())
  } catch {
    // The abort lands here too: a timed-out check learned nothing, which is the
    // same answer as offline.
    return null
  } finally {
    clearTimeout(timer)
  }
}

/**
 * Watches the origin for a build other than the one this bundle is (ADR 0087).
 *
 * Detection only — it decides nothing about reloading. What to do about an
 * available update is a policy question involving the user's unsent work and
 * whether they are even looking at the page, and that lives in
 * `src/update-refresh.ts`.
 */
export function createUpdateChecker(
  options: UpdateCheckerOptions,
): UpdateChecker {
  const {
    currentVersion,
    fetchManifest,
    intervalMs = DEFAULT_INTERVAL_MS,
    isVisible = () => true,
  } = options

  const available = signal(false)
  const latest = signal<VersionManifest | null>(null)
  const status = signal<UpdateStatus>('idle')

  let timer: ReturnType<typeof setInterval> | null = null
  /** Collapses overlapping checks — several triggers can fire at once. */
  let inFlight: Promise<void> | null = null
  /**
   * Bumped per check, so a run can tell whether it is still the current one.
   *
   * The watchdog on `check()` can free the de-duplication slot while a run is
   * still outstanding, and that run's continuation is not cancellable. Without
   * this it could resolve long afterwards and overwrite `status` / `latest` /
   * `available` with an answer that later checks have already superseded. Not
   * reachable through the shipped `fetchVersionManifest`, which aborts itself —
   * but `fetchManifest` is an injection point, and a stale write is cheaper to
   * make impossible than to reason about (raised in review of #114).
   */
  let generation = 0

  async function run(): Promise<void> {
    const mine = ++generation
    /** False once a later check has begun: this run's answer is stale. */
    const current = () => mine === generation

    status.value = 'checking'
    const manifest = await fetchManifest()
    if (!current()) {
      return
    }
    if (manifest === null) {
      status.value = 'error'
      return
    }
    latest.value = manifest
    if (manifest.version === currentVersion) {
      // Not `available.value = false`: the signal latches. Seeing our own
      // version again after a new one appeared means we are reading a server
      // that has rolled back, or an intermediary serving a stale manifest —
      // neither is a reason to un-tell the user about a build we did see.
      status.value = available.value ? 'available' : 'current'
      return
    }
    available.value = true
    status.value = 'available'
  }

  return {
    available,
    latest,
    status,

    check() {
      // `settleWithin`, not a bare `.finally`: this slot is what de-duplicates
      // every trigger, so a `fetchManifest` that never settles would not lose
      // one check — it would pin `inFlight` forever and every later
      // visibility/reconnect/interval/manual check would return the same stuck
      // promise, disabling update detection for the life of the tab. The
      // shipped `fetchVersionManifest` bounds itself, but `fetchManifest` is an
      // injection point and the slot should not depend on its manners.
      //
      // Freeing the slot does not cancel the run behind it — nothing can — so
      // the abandoned run is fenced off by `generation` instead, and cannot
      // write its late answer over a newer one.
      inFlight ??= settleWithin(run(), WATCHDOG_MS).finally(() => {
        inFlight = null
      })
      return inFlight
    },

    start() {
      if (timer !== null) {
        return
      }
      timer = setInterval(() => {
        // A hidden tab is either frozen (mobile, where the timer will not fire
        // at all) or about to be checked by the visibility handler when it
        // comes back. Polling it is pure waste.
        if (isVisible()) {
          void this.check()
        }
      }, intervalMs)
    },

    stop() {
      if (timer !== null) {
        clearInterval(timer)
        timer = null
      }
    },
  }
}
