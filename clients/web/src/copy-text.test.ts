import { afterEach, describe, expect, it, vi } from 'vitest'
import { copyText } from './copy-text'
import { perfMark } from './perf'

vi.mock('./perf', () => ({ perfMark: vi.fn() }))

afterEach(() => {
  // jsdom has no `execCommand`; the fallback test installs one.
  Reflect.deleteProperty(document, 'execCommand')
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
  vi.mocked(perfMark).mockClear()
})

describe('copyText', () => {
  it('writes through the clipboard API', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    expect(await copyText('hello', 'body-hold-touchend')).toBe(true)
    expect(writeText).toHaveBeenCalledWith('hello')
    expect(perfMark).toHaveBeenCalledWith('copy:ok', {
      source: 'body-hold-touchend',
      activation: null,
    })
  })

  it('returns false when the clipboard API is missing', async () => {
    vi.stubGlobal('navigator', {})
    expect(await copyText('hello')).toBe(false)
  })

  it('returns false when the write is rejected and the fallback fails', async () => {
    vi.stubGlobal('navigator', {
      clipboard: { writeText: vi.fn().mockRejectedValue(new Error('denied')) },
    })
    expect(await copyText('hello')).toBe(false)
  })

  it('falls back to execCommand when the write is rejected', async () => {
    const exec = vi.fn().mockReturnValue(true)
    Object.defineProperty(document, 'execCommand', {
      value: exec,
      configurable: true,
    })
    vi.stubGlobal('navigator', {
      clipboard: { writeText: vi.fn().mockRejectedValue(new Error('no')) },
    })
    expect(await copyText('hello', 'body-hold-release')).toBe(true)
    expect(exec).toHaveBeenCalledWith('copy')
    expect(document.querySelector('textarea')).toBeNull()
    const names = vi.mocked(perfMark).mock.calls.map((c) => c[0])
    expect(names).toEqual(['copy:failed', 'copy:fallback'])
    expect(vi.mocked(perfMark).mock.calls[1]![1]).toMatchObject({
      recovered: true,
    })
  })

  it('marks why a rejected write failed, without the text', async () => {
    const denied = Object.assign(new Error('secret body'), {
      name: 'NotAllowedError',
    })
    vi.stubGlobal('navigator', {
      clipboard: { writeText: vi.fn().mockRejectedValue(denied) },
      userActivation: { isActive: false, hasBeenActive: true },
    })
    expect(await copyText('secret body', 'body-hold-release')).toBe(false)
    const [name, detail] = vi.mocked(perfMark).mock.calls[0]!
    expect(name).toBe('copy:failed')
    expect(detail).toMatchObject({
      source: 'body-hold-release',
      reason: 'rejected',
      error: 'NotAllowedError',
      activation: false,
      everActive: true,
    })
    expect(JSON.stringify(detail)).not.toContain('secret')
  })

  it('merges caller context into the marks', async () => {
    const writeText = vi
      .fn()
      .mockResolvedValueOnce(undefined)
      .mockRejectedValueOnce(new Error('no'))
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    await copyText('a', 'body-hold-touchend', { heldMs: 640 })
    await copyText('b', 'body-hold-touchend', { heldMs: 910 })
    const marks = vi.mocked(perfMark).mock.calls
    expect(marks[0]![1]).toMatchObject({
      heldMs: 640,
      source: 'body-hold-touchend',
    })
    expect(marks[1]![1]).toMatchObject({ heldMs: 910, reason: 'rejected' })
  })

  it('marks a missing clipboard API', async () => {
    vi.stubGlobal('navigator', {})
    await copyText('hello', 'link-click')
    expect(vi.mocked(perfMark).mock.calls[0]![1]).toMatchObject({
      reason: 'no-api',
    })
  })
})
