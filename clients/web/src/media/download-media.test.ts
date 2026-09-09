import { afterEach, describe, expect, it, vi } from 'vitest'
import { browserPlatform } from '../platform'
import { downloadMedia, isDownloadable } from './download-media'
import type { MediaService } from './media-service'
import type { ParsedMedia } from './parse-media'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'

function media(overrides: Partial<ParsedMedia> = {}): ParsedMedia {
  return {
    kind: 'image',
    url: 'mxc://hs/abc',
    thumbnailUrl: null,
    filename: 'cat.png',
    caption: null,
    encrypted: false,
    mimetype: 'image/png',
    ...overrides,
  } as ParsedMedia
}

function service(blob: Blob | null): MediaService {
  return {
    fetchBlob: vi
      .fn()
      .mockResolvedValue(
        blob === null
          ? { ok: false, error: { kind: 'network' } }
          : { ok: true, blob },
      ),
  } as unknown as MediaService
}

/**
 * jsdom has no `share`/`canShare`, so these are assigned per test and cleared
 * afterwards. Typed loosely because the real signatures are not present in the
 * jsdom lib and the tests only need presence and return value.
 */
const nav = navigator as unknown as Record<string, unknown>

afterEach(() => {
  nav.share = undefined
  nav.canShare = undefined
  vi.restoreAllMocks()
})

describe('isDownloadable', () => {
  it('accepts an mxc object', () => {
    expect(isDownloadable(media())).toBe(true)
  })

  it('rejects a local echo with no mxc yet', () => {
    expect(isDownloadable(media({ url: null }))).toBe(false)
  })

  it('rejects a non-mxc url', () => {
    expect(isDownloadable(media({ url: 'https://hs/cat.png' }))).toBe(false)
  })
})

describe('downloadMedia', () => {
  it('reports failure when the object cannot be fetched', async () => {
    expect(
      await downloadMedia(service(null), ACCOUNT, media(), browserPlatform()),
    ).toBe('failed')
  })

  it('falls back to an anchor when the platform cannot share files', async () => {
    const click = vi.fn()
    vi.spyOn(document, 'createElement').mockImplementation((tag: string) => {
      const el = Object.assign(
        document.createElementNS(
          'http://www.w3.org/1999/xhtml',
          tag,
        ) as HTMLElement,
        {},
      )
      if (tag === 'a') {
        ;(el as HTMLAnchorElement).click = click
      }
      return el
    })

    const outcome = await downloadMedia(
      service(new Blob(['x'])),
      ACCOUNT,
      media(),
      browserPlatform(),
    )
    expect(outcome).toBe('saved')
    expect(click).toHaveBeenCalledOnce()
  })

  it('uses the share sheet when the platform takes files', async () => {
    const share = vi.fn().mockResolvedValue(undefined)
    nav.canShare = () => true
    nav.share = share

    const outcome = await downloadMedia(
      service(new Blob(['x'])),
      ACCOUNT,
      media(),
      browserPlatform(),
    )
    expect(outcome).toBe('shared')
    const shared = share.mock.calls[0][0] as { files: File[] }
    expect(shared.files[0].name).toBe('cat.png')
    expect(shared.files[0].type).toBe('image/png')
  })

  it('treats a dismissed share sheet as a cancel, not a failure', async () => {
    // Otherwise changing your mind shows an error.
    nav.canShare = () => true
    nav.share = vi
      .fn()
      .mockRejectedValue(new DOMException('cancelled', 'AbortError'))

    expect(
      await downloadMedia(
        service(new Blob(['x'])),
        ACCOUNT,
        media(),
        browserPlatform(),
      ),
    ).toBe('cancelled')
  })

  it('falls back to the anchor when the share sheet errors', async () => {
    // A sheet that cannot take the file must not leave the user with nothing.
    const click = vi.fn()
    vi.spyOn(document, 'createElement').mockImplementation((tag: string) => {
      const el = document.createElementNS(
        'http://www.w3.org/1999/xhtml',
        tag,
      ) as HTMLElement
      if (tag === 'a') {
        ;(el as HTMLAnchorElement).click = click
      }
      return el
    })
    nav.canShare = () => true
    nav.share = vi.fn().mockRejectedValue(new Error('not supported'))

    expect(
      await downloadMedia(
        service(new Blob(['x'])),
        ACCOUNT,
        media(),
        browserPlatform(),
      ),
    ).toBe('saved')
    expect(click).toHaveBeenCalledOnce()
  })

  it('does not offer the sheet when it declines this file', async () => {
    const share = vi.fn()
    const click = vi.fn()
    vi.spyOn(document, 'createElement').mockImplementation((tag: string) => {
      const el = document.createElementNS(
        'http://www.w3.org/1999/xhtml',
        tag,
      ) as HTMLElement
      if (tag === 'a') {
        ;(el as HTMLAnchorElement).click = click
      }
      return el
    })
    nav.canShare = () => false
    nav.share = share

    expect(
      await downloadMedia(
        service(new Blob(['x'])),
        ACCOUNT,
        media(),
        browserPlatform(),
      ),
    ).toBe('saved')
    expect(share).not.toHaveBeenCalled()
    expect(click).toHaveBeenCalledOnce()
  })
})
