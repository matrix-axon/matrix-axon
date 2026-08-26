import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import type { ParsedMedia } from '../media/parse-media'
import { ServicesContext } from '../services'
import { TEST_BASE_URL, testServices } from '../test/services'
import { MediaImage } from './MediaImage'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'
const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47])

function image(overrides: Partial<ParsedMedia> = {}): ParsedMedia {
  return {
    kind: 'image',
    url: 'mxc://hs/full',
    thumbnailUrl: null,
    filename: 'cat.png',
    caption: null,
    encrypted: false,
    mimetype: 'image/png',
    w: 800,
    h: 600,
    ...overrides,
  }
}

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
})
afterAll(() => server.close())

function serveBytes() {
  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/media/:account/:server/:media`,
      () => new HttpResponse(PNG, { headers: { 'content-type': 'image/png' } }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
      () => new HttpResponse(PNG, { headers: { 'content-type': 'image/png' } }),
    ),
  )
}

function renderImage(media: ParsedMedia, previewUrl?: string | null) {
  return render(
    <ServicesContext.Provider value={testServices()}>
      <MediaImage accountId={ACCOUNT} media={media} previewUrl={previewUrl} />
    </ServicesContext.Provider>,
  )
}

describe('MediaImage', () => {
  it('reserves the aspect ratio from info dimensions before the blob resolves', () => {
    serveBytes()
    const { container } = renderImage(image())
    const box = container.querySelector('.media-image') as HTMLElement
    expect(box.style.aspectRatio).toBe('800 / 600')
    // 800×600 scales to a 320px longest side → 320px wide, shrinkable.
    expect(box.style.width).toBe('320px')
    expect(box.style.maxWidth).toBe('100%')
  })

  it('caps a huge image to a thumbnail instead of the intrinsic width', () => {
    serveBytes()
    const { container } = renderImage(image({ w: 4000, h: 3000 }))
    const box = container.querySelector('.media-image') as HTMLElement
    // 4000×3000 → longest side 4000 scaled to 320 → 320px wide, not 4000px.
    expect(box.style.width).toBe('320px')
    expect(box.style.aspectRatio).toBe('4000 / 3000')
  })

  it('does not upscale an image smaller than the thumbnail bound', () => {
    serveBytes()
    const { container } = renderImage(image({ w: 100, h: 80 }))
    const box = container.querySelector('.media-image') as HTMLElement
    expect(box.style.width).toBe('100px')
  })

  it('keeps a known portrait thumbnail narrow instead of reserving a square frame', () => {
    serveBytes()
    const { container } = renderImage(image({ w: 400, h: 1200 }))
    const box = container.querySelector('.media-image') as HTMLElement
    // 400×1200 → longest side 1200 scaled to 320 → about 107px wide.
    expect(box.style.width).toBe('107px')
    expect(box.style.aspectRatio).toBe('400 / 1200')
  })

  it('caps a dimensionless image to the thumbnail box', () => {
    serveBytes()
    const { container } = renderImage(image({ w: undefined, h: undefined }))
    const box = container.querySelector('.media-image') as HTMLElement
    const thumbnail = container.querySelector('.media-thumbnail') as HTMLElement
    expect(thumbnail.classList.contains('media-thumbnail-unsized')).toBe(true)
    expect(box.style.width).toBe('320px')
    expect(box.style.maxWidth).toBe('100%')
    expect(box.style.maxHeight).toBe('320px')
  })

  it('holds a height for a dimensionless image, releasing it once loaded', async () => {
    serveBytes()
    const { container } = renderImage(image({ w: undefined, h: undefined }))
    // Nothing to derive a ratio from, so reserve a box rather than collapse to
    // zero height and snap to full size on decode, mid-scroll.
    expect(
      (container.querySelector('.media-image') as HTMLElement).style.minHeight,
    ).toBe('180px')

    // Once the image is up it sizes the box itself; a short image must not sit
    // in a tall empty frame.
    await waitFor(() =>
      expect(
        (container.querySelector('.media-image') as HTMLElement).style
          .minHeight,
      ).toBe(''),
    )
  })

  it('decodes off the main thread so a scroll gesture is not blocked', async () => {
    serveBytes()
    const { container } = renderImage(image())
    await waitFor(() =>
      expect(container.querySelector('.media-open img')).not.toBeNull(),
    )
    expect(
      container.querySelector('.media-open img')?.getAttribute('decoding'),
    ).toBe('async')
  })

  it('caps an uploading local preview to the thumbnail box', () => {
    const { container } = renderImage(
      image({ url: null, w: undefined, h: undefined }),
      'blob:preview',
    )
    const box = container.querySelector('.media-image') as HTMLElement
    const thumbnail = container.querySelector('.media-thumbnail') as HTMLElement
    const img = container.querySelector('img') as HTMLImageElement
    expect(thumbnail.classList.contains('media-thumbnail-unsized')).toBe(true)
    expect(box.style.width).toBe('320px')
    expect(box.style.maxWidth).toBe('100%')
    expect(box.style.maxHeight).toBe('320px')
    expect(img.className).toBe('media-preview')
    expect(img.src).toBe('blob:preview')
  })

  it('opens a dimensionless image in a lightbox outside the thumbnail frame', async () => {
    serveBytes()
    const { findByRole } = renderImage(image({ w: undefined, h: undefined }))
    const img = await findByRole('img')
    fireEvent.click(img)

    const dialog = await findByRole('dialog')
    expect(dialog.closest('.media-thumbnail')).toBeNull()
    expect(dialog.closest('.media-image')).toBeNull()
    expect(dialog.parentElement?.parentElement).toBe(document.body)
    await waitFor(() =>
      expect(document.body.querySelector('.lightbox-image img')).toBeTruthy(),
    )
  })

  it('renders markdown in the caption', () => {
    serveBytes()
    const { container } = renderImage(image({ caption: 'a **bold** caption' }))
    const caption = container.querySelector('.media-caption')
    expect(caption?.querySelector('strong')?.textContent).toBe('bold')
    expect(caption?.textContent).toBe('a bold caption')
  })

  it('renders a blob-backed image once the download resolves', async () => {
    serveBytes()
    const { findByRole } = renderImage(image())
    const img = (await findByRole('img')) as HTMLImageElement
    expect(img.src).toMatch(/^blob:/)
    expect(img.alt).toBe('cat.png')
  })

  it('uses a server-generated thumbnail for plaintext images without sender thumbnails', async () => {
    let thumbnailUrl: string | null = null
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
        ({ request }) => {
          thumbnailUrl = request.url
          return new HttpResponse(PNG, {
            headers: { 'content-type': 'image/png' },
          })
        },
      ),
    )

    const { findByRole } = renderImage(image())

    expect(await findByRole('img')).toBeTruthy()
    expect(thumbnailUrl).toBe(
      `${TEST_BASE_URL}/v1/media/${ACCOUNT}/hs/full/thumbnail?width=320&height=320&method=scale`,
    )
  })

  it('does not ask the server to generate thumbnails for encrypted images', async () => {
    let thumbnailFetches = 0
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media`,
        () =>
          new HttpResponse(PNG, { headers: { 'content-type': 'image/png' } }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
        () => {
          thumbnailFetches += 1
          return new HttpResponse(PNG, {
            headers: { 'content-type': 'image/png' },
          })
        },
      ),
    )

    const { findByRole } = renderImage(image({ encrypted: true }))

    expect(await findByRole('img')).toBeTruthy()
    expect(thumbnailFetches).toBe(0)
  })

  it('opens a lightbox on click and closes it on Escape', async () => {
    serveBytes()
    const { findByRole, queryByRole } = renderImage(image())
    const img = await findByRole('img')
    fireEvent.click(img)

    const dialog = await findByRole('dialog')
    expect(dialog).toBeTruthy()

    const escape = new KeyboardEvent('keydown', {
      key: 'Escape',
      bubbles: true,
      cancelable: true,
    })
    document.body.dispatchEvent(escape)
    await waitFor(() => expect(queryByRole('dialog')).toBeNull())
  })

  it('shows a failure placeholder when the download 404s', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
        () => HttpResponse.json({ error: {} }, { status: 404 }),
      ),
      http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
        HttpResponse.json({ error: {} }, { status: 404 }),
      ),
    )
    const { findByText } = renderImage(image())
    expect(await findByText('Could not load image')).toBeTruthy()
  })

  it('shows the ciphertext-fallback placeholder when the image fails to decode', async () => {
    serveBytes()
    const { findByRole, findByText } = renderImage(image({ encrypted: true }))
    const img = await findByRole('img')
    // The proxy returned a 200 of raw ciphertext; decode fails at the <img>.
    fireEvent.error(img)
    expect(
      await findByText('Encrypted media — server could not decrypt'),
    ).toBeTruthy()
  })

  it('falls back to the full-size image when the thumbnail fails (WCR-18)', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media`,
        ({ params }) =>
          params.media === 'thumb'
            ? new HttpResponse(null, { status: 404 })
            : new HttpResponse(PNG, {
                headers: { 'content-type': 'image/png' },
              }),
      ),
    )
    const { findByRole } = renderImage(
      image({ thumbnailUrl: 'mxc://hs/thumb' }),
    )
    // A broken sender-embedded thumbnail must not hide a loadable image.
    expect(await findByRole('img')).toBeTruthy()
  })

  it('falls back to the full-size image when generated thumbnailing fails', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
        () => new HttpResponse(null, { status: 404 }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media`,
        () =>
          new HttpResponse(PNG, { headers: { 'content-type': 'image/png' } }),
      ),
    )

    const { findByRole } = renderImage(image())

    expect(await findByRole('img')).toBeTruthy()
  })
})
