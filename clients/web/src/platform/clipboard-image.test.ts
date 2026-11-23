import { invoke } from '@tauri-apps/api/core'
import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  canFallBackToClipboardImage,
  readClipboardImage,
  supportsNativeImagePaste,
} from './clipboard-image'

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))

afterEach(() => {
  vi.unstubAllGlobals()
  vi.resetAllMocks()
})

describe('Linux clipboard image fallback', () => {
  it('requires both the Linux build target and the desktop shell', () => {
    vi.stubGlobal('__AXON_TAURI_PLATFORM__', 'linux')
    expect(supportsNativeImagePaste()).toBe(false)
    vi.stubGlobal('__TAURI_INTERNALS__', {})
    expect(supportsNativeImagePaste()).toBe(true)
    vi.stubGlobal('__AXON_TAURI_PLATFORM__', 'macos')
    expect(supportsNativeImagePaste()).toBe(false)
  })

  it.each([
    { types: [], text: '', expected: true },
    { types: ['Files', 'image/png'], text: '', expected: true },
    { types: [], text: 'plain text', expected: false },
    { types: [], text: 'https://example.org', expected: false },
    { types: ['text/html', 'Files'], text: '', expected: false },
    { types: ['text/uri-list'], text: '', expected: false },
  ])('checks both offered types and actual text ($types, $text)', (entry) => {
    expect(
      canFallBackToClipboardImage({ types: entry.types }, entry.text),
    ).toBe(entry.expected)
  })

  it('turns raw clipboard bytes into a correctly typed attachment', async () => {
    const bytes = Uint8Array.of(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)
    vi.mocked(invoke).mockResolvedValue(bytes.buffer)

    const file = await readClipboardImage()
    expect(invoke).toHaveBeenCalledWith('read_clipboard_image')
    expect(file).toMatchObject({
      name: 'clipboard.png',
      type: 'image/png',
      size: bytes.length,
    })
    expect(await file?.arrayBuffer()).toEqual(bytes.buffer)
  })

  it.each([[], [0x01, 0x02, 0x03]])(
    'ignores an empty or unrecognized response (%j)',
    async (...bytes) => {
      vi.mocked(invoke).mockResolvedValue(Uint8Array.from(bytes).buffer)
      expect(await readClipboardImage()).toBeNull()
    },
  )
})
