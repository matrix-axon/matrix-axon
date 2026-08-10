/**
 * Reloading the document to pick up a new build, without ever looping (ADR
 * 0087).
 *
 * A reload loop is a worse outage than the hang this feature fixes: it burns
 * battery, it cannot be escaped by reloading, and on an installed PWA there is
 * no address bar to escape to. Two things cause one — the origin serving a
 * `version.json` that disagrees with the bundle it actually serves (a partial
 * deploy, an intermediary caching one but not the other), and a chunk that 404s
 * for a reason a reload cannot fix. Both look identical from here: we reload,
 * and come back running the build we left.
 *
 * Two independent limits, because there are two distinct loops to stop:
 *
 * 1. **Per attempt.** A reload is recorded as the pair *(build we left, thing
 *    we were reloading toward)* and the same pair is never retried. On the next
 *    boot the pair record is either stale — we came back on a different build,
 *    so it worked — or still current, in which case that attempt is spent.
 *    Keying on the pair rather than the departed build alone is what keeps one
 *    bad manifest from disabling refresh for the rest of the session: a later,
 *    genuinely new build is a different target and gets its own attempt.
 *
 * 2. **Per window.** `spent` counts reloads across *every* build this tab has
 *    been through and deliberately survives settling a pair. Without it the
 *    budget would reset on each boot that reached a new bundle, and an
 *    inconsistent deployment — A naming C, C naming E, E naming A — would
 *    reload forever while never repeating a pair.
 *
 * `spent` decays rather than accumulating for the life of the tab, because an
 * installed PWA's session can outlast many legitimate deploys; a hard lifetime
 * cap would silently stop updating a long-lived tab. The window bounds the
 * *rate* instead, which is what distinguishes thrash from ordinary use.
 */

/** sessionStorage, not localStorage: the block is per-tab and per-session. */
const GUARD_KEY = 'axon:update-reload'

/**
 * Ceiling on automatic reloads per window, across all builds and targets.
 * Reaching it is not silent — the banner takes over, so the user still has a
 * way to apply the update.
 */
export const MAX_ATTEMPTS = 3

/**
 * How long the budget window runs. Long enough that a thrashing origin is held
 * to three reloads rather than hundreds; short enough that a tab left open for
 * days still picks up genuine deploys on its own.
 */
export const BUDGET_WINDOW_MS = 10 * 60_000

/** Target for a reload that has no version to name — a failed chunk load. */
export const CHUNK_TARGET = 'chunk'

export interface ReloadEnvironment {
  /** `null` when sessionStorage is unreachable (disabled, quota, sandboxed). */
  storage: Storage | null
  reload: () => void
  /** Injected so tests can drive the budget window. */
  now?: () => number
  /**
   * Where the decision is recorded. Defaults to `console.info`; tests pass a
   * collector, and passing a no-op silences it.
   *
   * A page that reloads itself with no trace is close to undebuggable — the
   * reload wipes the console, so by the time anyone looks, the only evidence
   * that it was deliberate is gone. Every decision this module makes, to reload
   * or to decline, says so.
   */
  log?: (message: string) => void
}

interface GuardState {
  /** The build that was running when `targets` were attempted. */
  from: string
  /** What each attempt *from this build* was reloading toward. */
  targets: string[]
  /** Automatic reloads spent in the current window, across every build. */
  spent: number
  /** When the current budget window opened, as epoch ms. */
  windowStartedAt: number
}

function browserStorage(): Storage | null {
  try {
    return window.sessionStorage
  } catch {
    return null
  }
}

export function browserReloadEnvironment(): ReloadEnvironment {
  return {
    storage: browserStorage(),
    reload: () => {
      window.location.reload()
    },
    now: () => Date.now(),
    log: (message) => console.info(message),
  }
}

function clock(env: ReloadEnvironment): number {
  return (env.now ?? (() => Date.now()))()
}

/** Prefixed so it is greppable in a console full of extension noise. */
function log(env: ReloadEnvironment, message: string): void {
  ;(env.log ?? ((line: string) => console.info(line)))(
    `[axon:update] ${message}`,
  )
}

function read(storage: Storage | null): GuardState | null {
  if (storage === null) {
    return null
  }
  let raw: string | null
  try {
    raw = storage.getItem(GUARD_KEY)
  } catch {
    return null
  }
  if (raw === null) {
    return null
  }
  try {
    const value: unknown = JSON.parse(raw)
    if (typeof value !== 'object' || value === null) {
      return null
    }
    const state = value as Record<string, unknown>
    if (typeof state.from !== 'string' || !Array.isArray(state.targets)) {
      return null
    }
    return {
      from: state.from,
      targets: state.targets.filter((t): t is string => typeof t === 'string'),
      // A record written before these fields existed reads as a fresh budget
      // rather than as a corrupt state that blocks every reload.
      spent: typeof state.spent === 'number' ? state.spent : 0,
      windowStartedAt:
        typeof state.windowStartedAt === 'number' ? state.windowStartedAt : 0,
    }
  } catch {
    // Unreadable guard state is treated as absent. It fails *open* — one
    // unguarded reload — which is safe only because the very next boot rewrites
    // it in a readable form, so this cannot repeat.
    return null
  }
}

/** Returns false when the write failed — the caller must not auto-reload then. */
function write(storage: Storage | null, state: GuardState): boolean {
  if (storage === null) {
    return false
  }
  try {
    storage.setItem(GUARD_KEY, JSON.stringify(state))
    return true
  } catch {
    return false
  }
}

/**
 * Settle the previous boot's guard. Call once at startup, before anything can
 * ask to reload: if we came back on a different build than the one those
 * attempts were made from, they worked, so the *pair* record is spent.
 *
 * The budget is deliberately **not** cleared here. Landing on a new bundle
 * proves the last reload did something, not that the deployment is consistent —
 * a manifest chain that names a different build every time would otherwise
 * refill the budget on every boot and reload without end.
 */
export function initReloadGuard(
  currentVersion: string,
  env: ReloadEnvironment,
): void {
  const state = read(env.storage)
  if (state === null || state.from === currentVersion) {
    return
  }
  write(env.storage, { ...state, from: currentVersion, targets: [] })
}

/**
 * Reload to pick up `target`, unless this exact attempt has already been made
 * from this build or the window's budget is spent. Returns whether it reloaded;
 * `false` means the caller should fall back to asking the user, which is always
 * allowed to reload — see `reloadNow`.
 *
 * A missing or unwritable sessionStorage blocks the reload rather than
 * proceeding unguarded. An automatic action we cannot bound is exactly what
 * this module exists to prevent, and the cost of failing closed is only that
 * the user sees a banner.
 */
export function reloadOnce(
  currentVersion: string,
  target: string,
  env: ReloadEnvironment,
): boolean {
  const now = clock(env)
  const previous = read(env.storage)
  const expired =
    previous !== null && now - previous.windowStartedAt >= BUDGET_WINDOW_MS
  const base: GuardState =
    previous === null
      ? { from: currentVersion, targets: [], spent: 0, windowStartedAt: now }
      : {
          ...previous,
          // A record from another build keeps its budget but not its pairs;
          // `initReloadGuard` normally does this, but a caller that skipped it
          // must not get a free attempt out of the omission.
          ...(previous.from === currentVersion
            ? {}
            : { from: currentVersion, targets: [] }),
          ...(expired ? { spent: 0, windowStartedAt: now } : {}),
        }

  if (base.targets.includes(target) || base.spent >= MAX_ATTEMPTS) {
    log(
      env,
      `declining to reload ${currentVersion} → ${target}: ` +
        (base.targets.includes(target)
          ? 'already attempted from this build'
          : `budget spent (${base.spent}/${MAX_ATTEMPTS} this window)`) +
        '. The banner is the remaining path.',
    )
    return false
  }
  const next: GuardState = {
    ...base,
    targets: [...base.targets, target],
    spent: base.spent + 1,
  }
  if (!write(env.storage, next)) {
    log(
      env,
      'declining to reload: sessionStorage is unwritable, so the loop guard ' +
        'cannot bound an automatic reload. The banner is the remaining path.',
    )
    return false
  }
  log(
    env,
    `reloading to pick up a new build: ${currentVersion} → ${target} ` +
      `(attempt ${next.spent}/${MAX_ATTEMPTS} this window)`,
  )
  env.reload()
  return true
}

/**
 * Reload because the user asked. No guard: they can see the outcome and decide
 * for themselves whether to try again, which is the whole reason the automatic
 * path is allowed to fail closed into this one.
 */
export function reloadNow(env: ReloadEnvironment): void {
  env.reload()
}
