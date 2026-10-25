import { afterEach, describe, expect, it, vi } from 'vitest'
import { copyText } from './copy-text'
import { writeText as nativeWriteText } from '@tauri-apps/plugin-clipboard-manager'
import { perfMark } from './perf'
import { isTauriRuntime } from './platform'

vi.mock('./perf', () => ({ perfMark: vi.fn() }))
vi.mock('./platform', () => ({ isTauriRuntime: vi.fn(() => false) }))
vi.mock('@tauri-apps/plugin-clipboard-manager', () => ({ writeText: vi.fn() }))

afterEach(() => {
  // jsdom has no `execCommand`; the fallback test installs one.
  Reflect.deleteProperty(document, 'execCommand')
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
  vi.mocked(perfMark).mockClear()
  vi.mocked(isTauriRuntime).mockReturnValue(false)
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

  describe('in the Tauri shell', () => {
    it('writes through the native plugin, not the webview', async () => {
      vi.mocked(isTauriRuntime).mockReturnValue(true)
      vi.mocked(nativeWriteText).mockResolvedValue(undefined)
      const writeText = vi.fn()
      vi.stubGlobal('navigator', { clipboard: { writeText } })
      expect(await copyText('hello', 'body-hold-touchend')).toBe(true)
      expect(nativeWriteText).toHaveBeenCalledWith('hello')
      expect(writeText).not.toHaveBeenCalled()
      expect(perfMark).toHaveBeenCalledWith('copy:ok', {
        source: 'body-hold-touchend',
        native: true,
      })
    })

    it('falls back to the web clipboard when the plugin refuses', async () => {
      vi.mocked(isTauriRuntime).mockReturnValue(true)
      vi.mocked(nativeWriteText).mockRejectedValue(new Error('not allowed'))
      const writeText = vi.fn().mockResolvedValue(undefined)
      vi.stubGlobal('navigator', { clipboard: { writeText } })
      expect(await copyText('hello')).toBe(true)
      expect(writeText).toHaveBeenCalledWith('hello')
      expect(perfMark).toHaveBeenCalledWith(
        'copy:native-failed',
        expect.objectContaining({ error: 'Error' }),
      )
    })
  })

  it('marks a missing clipboard API, and whether the fallback saved it', async () => {
    Object.defineProperty(document, 'execCommand', {
      value: vi.fn().mockReturnValue(true),
      configurable: true,
    })
    vi.stubGlobal('navigator', {})
    expect(await copyText('hello', 'link-click')).toBe(true)
    const marks = vi.mocked(perfMark).mock.calls
    expect(marks[0]![1]).toMatchObject({ reason: 'no-api' })
    expect(marks[1]).toEqual([
      'copy:fallback',
      { source: 'link-click', recovered: true },
    ])
  })
})
