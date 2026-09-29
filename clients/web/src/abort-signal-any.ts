/**
 * `AbortSignal.any` for WebViews that predate it.
 *
 * Chrome shipped it in 116 (WebKit in 17.4). Android System WebView updates
 * through Play, but not on every device: an emulator image, a device with
 * updates disabled, or a de-Googled one runs whatever it shipped with — an
 * API 33 image carries Chrome 109. There the property is `undefined`, so
 * `boundedSignal` and the API client throw a `TypeError` on their first
 * request. `ServerSetup` folds every failure into "Could not reach <server>",
 * so a healthy server looked down, and after sign-in the same error surfaced as
 * "AbortSignal.any is not a function".
 *
 * Installed once, before anything can ask for it, and only when the native one
 * is missing — a browser that has it keeps its own, which composes the reasons
 * and, per the spec, does not hold its sources alive.
 */
export function installAbortSignalAny(target: typeof AbortSignal): void {
  if (typeof target.any === 'function') return

  target.any = (signals: Iterable<AbortSignal>): AbortSignal => {
    const controller = new AbortController()
    const sources = [...signals]
    const aborted = sources.find((source) => source.aborted)
    if (aborted) {
      controller.abort(aborted.reason)
      return controller.signal
    }
    const onAbort = (event: Event): void => {
      controller.abort((event.target as AbortSignal).reason)
      // The first source to abort settles the result; drop the rest so a
      // long-lived signal does not keep this closure and controller alive.
      //
      // That is the only release there is: a combined signal that is never
      // aborted leaves one listener on every source. Both callers
      // (`boundedSignal`, `withDeadline`) include an `AbortSignal.timeout`, so
      // each combined signal aborts at the latest when its deadline fires, and
      // the listeners it added go with it — held for at most one deadline
      // (120 s in `boundedSignal`), not for the session. The native
      // implementation holds its sources weakly and needs none of this; doing
      // the same here would let a signal chain be collected mid-request, which
      // is the WebKit failure `api/client.ts` documents. A caller that combined
      // signals with no deadline would leak here, so don't.
      for (const source of sources) source.removeEventListener('abort', onAbort)
    }
    for (const source of sources) {
      source.addEventListener('abort', onAbort, { once: true })
    }
    return controller.signal
  }
}
