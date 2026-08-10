import { consumeScriptedReload } from './reload'

type NavigationTimingEntry = {
  type?: string
}

/**
 * Navigation Timing Level 1 — deprecated, gone from modern engines, and the only
 * navigation signal on the ones that never shipped Level 2.
 *
 * Its fields are `unknown` on purpose. A partial shim (`{}`, or a privacy build
 * that keeps the object but drops the constants) is a real shape in the wild,
 * and comparing two `undefined`s would read as a reload.
 */
type LegacyNavigation = {
  type?: unknown
  TYPE_RELOAD?: unknown
  TYPE_BACK_FORWARD?: unknown
}

export type NavigationPerformance = {
  getEntriesByType(type: string): PerformanceEntryList | NavigationTimingEntry[]
  navigation?: LegacyNavigation
}

/** Returns the Navigation Timing Level 2 entry type when the engine has one. */
export function navigationTimingType(
  performance: NavigationPerformance | undefined,
): string | undefined {
  try {
    return (
      performance?.getEntriesByType('navigation')[0] as
        NavigationTimingEntry | undefined
    )?.type
  } catch {
    return undefined
  }
}

/**
 * Whether Navigation Timing Level 1 reports a reload or a history restore.
 *
 * Absent, partially shimmed, and unreadable all read as `false`. On a hardened
 * webview the `navigation` property access itself throws, and this is consumed
 * from a `useLayoutEffect` with no error boundary above it — degrading to "not a
 * reload" keeps the deep link, while throwing would lose the whole render.
 */
export function legacyReloadOrRestore(
  performance: NavigationPerformance | undefined,
): boolean {
  try {
    const legacy = performance?.navigation
    if (typeof legacy?.type !== 'number') {
      return false
    }
    return (
      legacy.type === legacy.TYPE_RELOAD ||
      legacy.type === legacy.TYPE_BACK_FORWARD
    )
  } catch {
    return false
  }
}

/** Everything that can answer "how did this document get here?". */
export interface NavigationSignals {
  /** Navigation Timing Level 2 `type`; `undefined` when the engine has none. */
  timingType: string | undefined
  /** The Level 1 reading, consulted only when there is no Level 2 entry. */
  legacyReloadOrRestore: boolean
  /** A reload Axon performed itself, marked by `reload.ts` before reloading. */
  scriptedReload: boolean
}

/**
 * `globalThis.performance` rather than the bare global: an engine without it
 * would throw a ReferenceError while this module initializes, which is a blank
 * page rather than a degraded one.
 */
const bootPerformance: NavigationPerformance | undefined =
  globalThis.performance

/**
 * How this document arrived, sampled once at boot.
 *
 * Sampled at module scope for two reasons. Startup routing and performance
 * telemetry share one reading, so their view of the document cannot drift. And
 * the scripted-reload marker is consumed exactly once per document — not from a
 * component effect, because a boot that never mounts the signed-in shell (a
 * chunk-load reload on the sign-in screen) would leave the marker in
 * sessionStorage for a later, genuine deep-link navigation in the same tab to
 * pick up.
 *
 * Every reader is guarded, so initializing this cannot throw.
 */
export const bootNavigationSignals: NavigationSignals = {
  timingType: navigationTimingType(bootPerformance),
  legacyReloadOrRestore: legacyReloadOrRestore(bootPerformance),
  scriptedReload: consumeScriptedReload(),
}

/**
 * Whether this document arrived by reload or history restoration, and so must
 * not restore UI state that only made sense in the document before it.
 *
 * The decision table, in order:
 *
 * | Signals | Result | Why |
 * |---|---|---|
 * | Axon reloaded us | reload | Firefox reports `location.reload()` as `navigate`, so the engine cannot be asked. |
 * | L2 `reload`/`back_forward` | reload | The standard signal every modern engine answers. |
 * | L2 anything else | navigation | An explicit modern answer is authoritative; a real `?thread=` deep link survives. |
 * | no L2 entry, or it throws | L1 decides | The behaviour every engine had before this stack. |
 * | no L1 either | navigation | Nothing claims this was a reload. |
 *
 * Deliberately engine-neutral: no user-agent test anywhere in this path. The one
 * browser-specific quirk this fix exists for — Firefox misreporting a *scripted*
 * reload as `navigate` — is answered by the marker, which is true on every
 * engine and needs no UA string to be believed. Firefox classifies a native
 * reload correctly, so the standard signals still cover Ctrl-R there.
 */
export function isReloadOrRestoreNavigation(
  signals: NavigationSignals = bootNavigationSignals,
): boolean {
  if (signals.scriptedReload) {
    return true
  }
  if (signals.timingType !== undefined) {
    return (
      signals.timingType === 'reload' || signals.timingType === 'back_forward'
    )
  }
  return signals.legacyReloadOrRestore
}
