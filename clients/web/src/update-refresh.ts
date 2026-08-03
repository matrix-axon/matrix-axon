import { effect } from '@preact/signals'
import {
  browserReloadEnvironment,
  reloadOnce,
  type ReloadEnvironment,
} from './reload'
import { settleWithin } from './settle-within'
import type { UpdateChecker } from './stores/update-check'

/**
 * How long the document must have been hidden for a return to count as "the
 * user was away", and therefore for a reload to be free. Short hides are tab
 * switches, a notification shade, an autofill sheet — reloading through one of
 * those is indistinguishable from the app crashing.
 */
export const AWAY_MS = 60_000

/**
 * Cap on the pre-reload draft flush. Every outbound call gets a timeout
 * (repo-wide boundary rule); here the failure mode is specific — the reload we
 * are about to do is the fix for an app that is *already* wedged, so a
 * server that never answers must not be what stops it. Losing at most the last
 * 800 ms debounce window of typing beats not reloading at all.
 */
export const FLUSH_TIMEOUT_MS = 2000

/** Re-check at most this often on visibility changes, to bound rapid switching. */
const VISIBILITY_THROTTLE_MS = 30_000

export interface AutoRefreshOptions {
  updates: UpdateChecker
  /** This bundle's build id, for the reload loop guard. */
  currentVersion: string
  /** Flush durable state (drafts) before reloading. */
  flush: () => Promise<void>
  /** True while a reload would destroy something only held in memory. */
  hasUnsentWork: () => boolean
  /** Injected in tests; defaults to the real document. */
  doc?: Document
  /** Injected in tests; defaults to window. */
  win?: Window
  env?: ReloadEnvironment
  now?: () => number
}

/**
 * The policy half of automatic refresh (ADR 0087): decide whether an available
 * update should reload the page now, or wait behind a banner.
 *
 * Reloading is free exactly when the user is not looking and has nothing
 * unsent. That is the case this exists for — an installed PWA resumed from the
 * background, which today has to be force-killed to come back working. When
 * the user *is* looking, we never take the page out from under them: the
 * banner in the shell is the whole of the interactive path.
 *
 * Returns a disposer; the app graph keeps it for the document's life.
 */
export function startAutoRefresh(options: AutoRefreshOptions): () => void {
  const {
    updates,
    currentVersion,
    flush,
    hasUnsentWork,
    doc = document,
    win = window,
    env = browserReloadEnvironment(),
    now = () => Date.now(),
  } = options

  /** When the document last became hidden; `null` while visible. */
  let hiddenAt: number | null = doc.hidden ? now() : null
  let lastCheckAt = 0
  let reloading = false

  async function reloadIfFree(awayFor: number): Promise<void> {
    if (reloading || !updates.available.peek()) {
      return
    }
    // An unsent echo lives only in memory; a reload would take the user's
    // message (or the `File` behind an upload) with it. Let the banner ask.
    if (hasUnsentWork()) {
      return
    }
    // Either they are not looking right now, or they were gone long enough
    // that the page they return to is not the page they left.
    if (!doc.hidden && awayFor < AWAY_MS) {
      return
    }
    const target = updates.latest.peek()?.version
    if (target === undefined) {
      return
    }
    reloading = true
    await settleWithin(flush(), FLUSH_TIMEOUT_MS)
    if (!reloadOnce(currentVersion, target, env)) {
      // Guard refused (this exact attempt was already made from this build, the
      // session is out of budget, or sessionStorage is unusable). Leave the
      // banner as the only path.
      reloading = false
    }
  }

  function checkThen(awayFor: number): void {
    const at = now()
    if (at - lastCheckAt < VISIBILITY_THROTTLE_MS) {
      // Too soon to re-ask the origin, but a known-available update still gets
      // its chance — this return may be the first moment reloading is free.
      void reloadIfFree(awayFor)
      return
    }
    lastCheckAt = at
    void updates.check().then(() => reloadIfFree(awayFor))
  }

  const onVisibilityChange = () => {
    if (doc.hidden) {
      hiddenAt = now()
      return
    }
    const awayFor = hiddenAt === null ? 0 : now() - hiddenAt
    hiddenAt = null
    checkThen(awayFor)
  }

  const onOnline = () => {
    checkThen(0)
  }

  doc.addEventListener('visibilitychange', onVisibilityChange)
  win.addEventListener('online', onOnline)

  // A tab that goes stale while hidden should come back already fixed rather
  // than reload in front of the user. On desktop the interval keeps running
  // while hidden, so this fires there; on mobile the page is frozen and the
  // visibility handler above is what does the work.
  const disposeEffect = effect(() => {
    if (updates.available.value && doc.hidden) {
      void reloadIfFree(Number.POSITIVE_INFINITY)
    }
  })

  updates.start()
  // One check at startup: this tab may have been restored from the back/forward
  // cache or opened from a home-screen icon onto an already-superseded build.
  checkThen(0)

  return () => {
    doc.removeEventListener('visibilitychange', onVisibilityChange)
    win.removeEventListener('online', onOnline)
    disposeEffect()
    updates.stop()
  }
}
