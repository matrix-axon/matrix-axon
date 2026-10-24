import { renderHook } from '@testing-library/preact'
import { expect, it } from 'vitest'
import type { ParsedMedia } from './parse-media'
import { useThumbnailFallback } from './use-thumbnail-fallback'

const media: ParsedMedia = {
  kind: 'sticker',
  url: 'mxc://hs/animated',
  thumbnailUrl: 'mxc://hs/static-preview',
  filename: 'Wave',
  caption: null,
  encrypted: false,
}

it('uses original sticker bytes in both inline and gallery views, including sender thumbnails', () => {
  for (const options of [{}, { size: 160, method: 'crop' as const }]) {
    const { result, unmount } = renderHook(() =>
      useThumbnailFallback(media, 'ready', options),
    )
    expect(result.current).toEqual({
      displayUrl: media.url,
      thumbnail: undefined,
    })
    unmount()
  }
})

it('retains thumbnail requests for ordinary image gallery cells', () => {
  const { result } = renderHook(() =>
    useThumbnailFallback({ ...media, kind: 'image' }, 'ready', {
      size: 160,
      method: 'crop',
    }),
  )
  expect(result.current).toEqual({
    displayUrl: media.url,
    thumbnail: { width: 160, height: 160, method: 'crop' },
  })
})
