/**
 * Resolve when `work` settles or when `ms` elapses, whichever comes first.
 *
 * A watchdog for promises whose *settling* is load-bearing rather than their
 * value — where a promise that never settles leaves something wedged rather
 * than merely slow. Both callers are that shape (ADR 0087): the pre-reload
 * draft flush, which must not be what stops a reload from fixing a broken page,
 * and the update check's de-duplication slot, which never clears if its work
 * hangs and so disables update detection for the tab's whole life.
 *
 * Rejections are swallowed, exactly like the timeout: to a watchdog "it failed"
 * and "it took too long" are the same answer — stop waiting. Callers that need
 * to tell them apart should not use this.
 */
export function settleWithin(
  work: Promise<unknown>,
  ms: number,
): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms)
    void work
      .catch(() => {})
      .finally(() => {
        clearTimeout(timer)
        resolve()
      })
  })
}
