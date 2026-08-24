import { afterEach, describe, expect, it, vi } from 'vitest'
import { copyText } from './copy-text'

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('copyText', () => {
  it('writes through the clipboard API', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    expect(await copyText('hello')).toBe(true)
    expect(writeText).toHaveBeenCalledWith('hello')
  })

  it('returns false when the clipboard API is missing', async () => {
    vi.stubGlobal('navigator', {})
    expect(await copyText('hello')).toBe(false)
  })

  it('returns false when the write is rejected', async () => {
    vi.stubGlobal('navigator', {
      clipboard: { writeText: vi.fn().mockRejectedValue(new Error('denied')) },
    })
    expect(await copyText('hello')).toBe(false)
  })
})
