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
 * So a reload attempt is recorded as the pair *(build we left, thing we were
 * reloading toward)*, and the same pair is never attempted twice. On the next
 * boot the record is either stale — we came back on a different build, so it
 * worked — or still current, in which case that particular attempt is spent.
 *
 * Keying on the pair rather than on the departed build alone is what keeps one
 * bad manifest from disabling automatic refresh for the rest of the session: a
 * later, genuinely new build is a different target and gets its own attempt.
 * `MAX_ATTEMPTS` then bounds the pathological case — an origin flapping between
 * many distinct versions, where every target is new.
 */

/** sessionStorage, not localStorage: the block is per-tab and per-session. */
const GUARD_KEY = 'axon:update-reload'

/**
 * Ceiling on automatic reloads per tab session, across all targets. The
 * per-pair rule already stops a repeating loop; this bounds a server that keeps
 * naming *new* versions it does not serve. Reaching it is not silent — the
 * banner takes over, so the user still has a way to apply the update.
 */
export const MAX_ATTEMPTS = 3

/** Target for a reload that has no version to name — a failed chunk load. */
export const CHUNK_TARGET = 'chunk'

export interface ReloadEnvironment {
  /** `null` when sessionStorage is unreachable (disabled, quota, sandboxed). */
  storage: Storage | null
  reload: () => void
}

interface GuardState {
  /** The build that was running when these attempts were made. */
  from: string
  /** What each attempt was reloading toward. */
  targets: string[]
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
  }
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

function remove(storage: Storage | null): void {
  try {
    storage?.removeItem(GUARD_KEY)
  } catch {
    // A guard we cannot clear only ever fails closed (no auto-reload).
  }
}

/**
 * Settle the previous boot's guard. Call once at startup, before anything can
 * ask to reload: if we came back on a different build than the one those
 * attempts were made from, they worked and the record is spent.
 */
export function initReloadGuard(
  currentVersion: string,
  env: ReloadEnvironment,
): void {
  const state = read(env.storage)
  if (state !== null && state.from !== currentVersion) {
    remove(env.storage)
  }
}

/**
 * Reload to pick up `target`, unless this exact attempt has already been made
 * from this build or the session has spent its budget. Returns whether it
 * reloaded; `false` means the caller should fall back to asking the user, which
 * is always allowed to reload — see `reloadNow`.
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
  const previous = read(env.storage)
  const state: GuardState =
    previous !== null && previous.from === currentVersion
      ? previous
      : { from: currentVersion, targets: [] }

  if (state.targets.includes(target) || state.targets.length >= MAX_ATTEMPTS) {
    return false
  }
  if (!write(env.storage, { ...state, targets: [...state.targets, target] })) {
    return false
  }
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
