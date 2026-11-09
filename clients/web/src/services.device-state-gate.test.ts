/**
 * `deviceStateFullReadGate` — a marker scope with no cursor waits for the room
 * list's first refresh before its full read (ADR 0131); drafts never wait.
 */
import { describe, expect, it, vi } from 'vitest'
import { deviceStateFullReadGate } from './services'
import {
  DRAFTS_NAMESPACE,
  READ_MARKERS_NAMESPACE,
  THREAD_READ_MARKERS_NAMESPACE,
} from './stores/device-state'

describe('deviceStateFullReadGate', () => {
  it('holds the marker namespaces behind the room list', async () => {
    let settle!: () => void
    const loaded = new Promise<void>((resolve) => {
      settle = resolve
    })
    const ensureLoaded = vi.fn(() => loaded)
    const gate = deviceStateFullReadGate({ ensureLoaded })

    let opened = 0
    void gate(READ_MARKERS_NAMESPACE).then(() => (opened += 1))
    void gate(THREAD_READ_MARKERS_NAMESPACE).then(() => (opened += 1))
    await Promise.resolve()
    expect(ensureLoaded).toHaveBeenCalledTimes(2)
    expect(opened).toBe(0)

    settle()
    await loaded
    await Promise.resolve()
    expect(opened).toBe(2)
  })

  it('never holds drafts', async () => {
    const ensureLoaded = vi.fn(() => new Promise<void>(() => {}))
    await deviceStateFullReadGate({ ensureLoaded })(DRAFTS_NAMESPACE)
    expect(ensureLoaded).not.toHaveBeenCalled()
  })
})
