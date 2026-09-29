import { afterEach, describe, expect, it, vi } from 'vitest'
import { installAbortSignalAny } from './abort-signal-any'

const native = AbortSignal.any

/** Remove the native implementation, as on a Chrome 109 WebView. */
function withoutNative(): void {
  Object.defineProperty(AbortSignal, 'any', {
    value: undefined,
    configurable: true,
    writable: true,
  })
}

afterEach(() => {
  Object.defineProperty(AbortSignal, 'any', {
    value: native,
    configurable: true,
    writable: true,
  })
})

describe('installAbortSignalAny', () => {
  it('leaves a native implementation alone', () => {
    installAbortSignalAny(AbortSignal)
    expect(AbortSignal.any).toBe(native)
  })

  it('installs one where it is missing', () => {
    withoutNative()
    expect(typeof AbortSignal.any).toBe('undefined')
    installAbortSignalAny(AbortSignal)
    expect(typeof AbortSignal.any).toBe('function')
  })

  it('aborts when any source aborts, carrying its reason', () => {
    withoutNative()
    installAbortSignalAny(AbortSignal)
    const a = new AbortController()
    const b = new AbortController()
    const combined = AbortSignal.any([a.signal, b.signal])
    expect(combined.aborted).toBe(false)
    b.abort('because')
    expect(combined.aborted).toBe(true)
    expect(combined.reason).toBe('because')
  })

  it('is already aborted if a source already is', () => {
    withoutNative()
    installAbortSignalAny(AbortSignal)
    const done = AbortSignal.abort('early')
    const combined = AbortSignal.any([new AbortController().signal, done])
    expect(combined.aborted).toBe(true)
    expect(combined.reason).toBe('early')
  })

  it('composes with AbortSignal.timeout, as boundedSignal does', async () => {
    withoutNative()
    installAbortSignalAny(AbortSignal)
    const caller = new AbortController()
    const combined = AbortSignal.any([caller.signal, AbortSignal.timeout(20)])
    await new Promise((resolve) => setTimeout(resolve, 60))
    expect(combined.aborted).toBe(true)
    expect((combined.reason as DOMException).name).toBe('TimeoutError')
    expect(caller.signal.aborted).toBe(false)
  })

  it('never aborts for an empty list', () => {
    withoutNative()
    installAbortSignalAny(AbortSignal)
    expect(AbortSignal.any([]).aborted).toBe(false)
  })

  // Review feedback on the polyfill: a combined signal that is never aborted
  // leaves a listener on each source. True until the deadline fires, and both
  // call sites (`boundedSignal`, `withDeadline`) always include one, so the
  // listener count is bounded by the requests inside one deadline window rather
  // than growing for the life of the session. This pins both halves.
  describe('listener lifetime', () => {
    const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

    it('holds a listener on a long-lived source until the combined signal aborts', () => {
      withoutNative()
      installAbortSignalAny(AbortSignal)
      const longLived = new AbortController()
      const add = vi.spyOn(longLived.signal, 'addEventListener')
      const remove = vi.spyOn(longLived.signal, 'removeEventListener')
      for (let i = 0; i < 50; i++) {
        AbortSignal.any([longLived.signal, AbortSignal.timeout(20_000)])
      }
      expect(add).toHaveBeenCalledTimes(50)
      expect(remove).not.toHaveBeenCalled()
    })

    it('releases every one of them when the bounding timeout fires', async () => {
      withoutNative()
      installAbortSignalAny(AbortSignal)
      const longLived = new AbortController()
      const add = vi.spyOn(longLived.signal, 'addEventListener')
      const remove = vi.spyOn(longLived.signal, 'removeEventListener')
      for (let i = 0; i < 50; i++) {
        AbortSignal.any([longLived.signal, AbortSignal.timeout(20)])
      }
      await sleep(80)
      expect(add).toHaveBeenCalledTimes(50)
      expect(remove).toHaveBeenCalledTimes(50)
    })
  })
})
