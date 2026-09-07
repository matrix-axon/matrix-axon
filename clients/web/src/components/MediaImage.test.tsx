import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import type { ParsedMedia } from '../media/parse-media'
import { ServicesContext } from '../services'
import { TEST_BASE_URL, testServices } from '../test/services'
import { MediaImage } from './MediaImage'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'
// Real magic bytes, not a prefix: the media service sniffs the head of every
// object it fetches, and a truncated signature would sniff as "no known
// format" — the one verdict these fixtures must not accidentally produce.
const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
const HEIC = new Uint8Array([
  0x00, 0x00, 0x00, 0x18, 0x66, 0x74, 0x79, 0x70, 0x68, 0x65, 0x69, 0x63,
])
/** Bytes matching no image container the sniffer knows. */
const CIPHERTEXT = new Uint8Array([
  0x3f, 0x91, 0xd2, 0x0a, 0x7c, 0x44, 0xe8, 0x16, 0x5b, 0x9a, 0x02, 0xff,
])

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

function serveBytes(bytes: Uint8Array = PNG) {
  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/media/:account/:server/:media`,
      () =>
        new HttpResponse(bytes, { headers: { 'content-type': 'image/png' } }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
      () =>
        new HttpResponse(bytes, { headers: { 'content-type': 'image/png' } }),
    ),
  )
}

/**
 * Fail the `<img>` decode for real: the first error only buys a re-fetch, so a
 * test that wants the failure placeholder has to exhaust the automatic retry.
 * Re-queries between the two, because the retry mints a new object URL and the
 * element rendered against the old one is gone.
 */
async function failDecodeTwice(
  findByRole: (role: string) => Promise<HTMLElement>,
): Promise<string> {
  const first = await findByRole('img')
  const firstSrc = first.getAttribute('src')
  fireEvent.error(first)
  const second = await waitFor(async () => {
    const img = await findByRole('img')
    expect(img.getAttribute('src')).not.toBe(firstSrc)
    return img
  })
  fireEvent.error(second)
  return second.getAttribute('src') ?? ''
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

  it('stays generic for unidentifiable bytes instead of accusing the server', async () => {
    // These could be a format the sniffer does not carry, a JSON error body,
    // or ciphertext. The client cannot tell which, so it says none of them and
    // reports only that the image would not display (#359).
    serveBytes(CIPHERTEXT)
    const { findByRole, findByText, queryByText } = renderImage(
      image({ encrypted: true, mimetype: undefined, filename: 'blob' }),
    )
    await failDecodeTwice(findByRole)
    expect(await findByText('Could not display this image')).toBeTruthy()
    expect(queryByText(/decrypt/)).toBeNull()
    // Download stays available. #328's review withheld it here on the grounds
    // that unidentifiable bytes are probably the proxy's ciphertext-fallback
    // 200; that path is near-unreachable (the proxy 404s or 502s instead), so
    // these are far more likely a format no table carries — where opening them
    // elsewhere is the whole remedy.
    expect(await findByRole('button', { name: 'Download' })).toBeTruthy()
  })

  it('offers download for unidentifiable plaintext bytes too', async () => {
    // Encryption never enters into it: bytes arrived either way, and another
    // application is the only thing left that might open them.
    serveBytes(CIPHERTEXT)
    const { findByRole, findByText } = renderImage(
      image({ mimetype: undefined, filename: 'blob' }),
    )
    await failDecodeTwice(findByRole)
    expect(await findByText('Could not display this image')).toBeTruthy()
    expect(await findByRole('button', { name: 'Download' })).toBeTruthy()
  })

  it('keeps the sender-declared name when the sniffer does not carry it', async () => {
    // The regression #360's first round introduced: `sniff.ts` has no DNG, so
    // these bytes sniff as `null`. Treating that as a decryption failure lost
    // both the format name and the Download that ADR 0101 had put there.
    serveBytes(CIPHERTEXT)
    const { findByRole, findByText } = renderImage(
      image({ mimetype: 'image/x-adobe-dng', filename: 'IMG_0007.dng' }),
    )
    await failDecodeTwice(findByRole)
    expect(
      await findByText("DNG image — this browser can't display it"),
    ).toBeTruthy()
    expect(await findByRole('button', { name: 'Download' })).toBeTruthy()
  })

  it('names the format instead of blaming decryption for a HEIC', async () => {
    // The whole point of ADR 0101: an iPhone photo arrives intact and simply
    // will not decode outside WebKit. Reporting that as a decryption failure
    // sent a real investigation after the wrong thing.
    serveBytes(HEIC)
    const { findByRole, findByText, queryByText } = renderImage(
      image({ mimetype: 'image/heic', filename: 'IMG_4021.HEIC' }),
    )
    await failDecodeTwice(findByRole)
    expect(
      await findByText("HEIC image — this browser can't display it"),
    ).toBeTruthy()
    expect(queryByText('Encrypted media — server could not decrypt')).toBeNull()
  })

  it('names the format from the bytes when the sender declared none', async () => {
    // ADR 0072 found real events carrying `application/octet-stream`. The
    // filename used to be the only signal; the bytes are a better one, and
    // they are right even when the extension lies.
    serveBytes(HEIC)
    const { findByRole, findByText } = renderImage(
      image({
        mimetype: 'application/octet-stream',
        filename: 'holiday-snap.jpg',
      }),
    )
    await failDecodeTwice(findByRole)
    expect(
      await findByText("HEIC image — this browser can't display it"),
    ).toBeTruthy()
  })

  it('offers a download from the failure placeholder so the bytes are reachable', async () => {
    serveBytes(HEIC)
    const { findByRole, findByText } = renderImage(
      image({ mimetype: 'image/heic', filename: 'IMG_4021.HEIC' }),
    )
    await failDecodeTwice(findByRole)
    await findByText("HEIC image — this browser can't display it")

    // Count the fetch rather than assert on the button settling: the button is
    // enabled both before the click and after it finishes, so a state check
    // would pass even if the handler never ran.
    let fetched = 0
    server.use(
      http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () => {
        fetched += 1
        return new HttpResponse(PNG, {
          headers: { 'content-type': 'image/heic' },
        })
      }),
    )
    fireEvent.click(await findByRole('button', { name: 'Download' }))
    // `downloadMedia` deliberately re-fetches rather than reuse the displayed
    // object URL, so a successful save is exactly one more request.
    await waitFor(() => expect(fetched).toBe(1))
  })

  it('reports a failed download rather than looking like it worked', async () => {
    serveBytes(HEIC)
    const { findByRole, findByText } = renderImage(
      image({ mimetype: 'image/heic', filename: 'IMG_4021.HEIC' }),
    )
    await failDecodeTwice(findByRole)
    await findByText("HEIC image — this browser can't display it")
    // The download re-fetches, so it is this request that fails — not the one
    // that delivered the bytes we could not decode.
    server.use(
      http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
        HttpResponse.json({ error: {} }, { status: 500 }),
      ),
    )
    fireEvent.click(await findByRole('button', { name: 'Download' }))
    expect(await findByText('Download failed')).toBeTruthy()
  })

  it('does not re-serve a failed object after the row remounts', async () => {
    // The reader leaves the room and comes back. `attempt` is component-local
    // and restarts at 0, while the service keeps released entries in its LRU —
    // so without invalidating on failure, both the fresh load and its automatic
    // retry are answered from cache with the two objects already known to be
    // broken, and no request leaves the browser.
    //
    // One `testServices()` for both mounts on purpose: a second `renderImage`
    // would build a new service with an empty cache and prove nothing.
    let fetched = 0
    server.use(
      http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () => {
        fetched += 1
        return new HttpResponse(PNG, {
          headers: { 'content-type': 'image/png' },
        })
      }),
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
        () => {
          fetched += 1
          return new HttpResponse(PNG, {
            headers: { 'content-type': 'image/png' },
          })
        },
      ),
    )
    const services = testServices()
    const mount = () =>
      render(
        <ServicesContext.Provider value={services}>
          <MediaImage accountId={ACCOUNT} media={image()} />
        </ServicesContext.Provider>,
      )

    const first = mount()
    await failDecodeTwice(first.findByRole)
    const afterFailures = fetched
    expect(afterFailures).toBeGreaterThanOrEqual(2)
    first.unmount()

    const second = mount()
    expect(await second.findByRole('img')).toBeTruthy()
    expect(fetched).toBeGreaterThan(afterFailures)
  })

  it('recovers a transient decode failure on the automatic retry', async () => {
    // The iPhone case behind issue #359: perfectly good bytes, an <img> that
    // fell over once. One re-fetch and the picture is back — the reader never
    // learns anything went wrong.
    serveBytes()
    const { findByRole, queryByText } = renderImage(image())
    const first = await findByRole('img')
    const firstSrc = first.getAttribute('src')
    fireEvent.error(first)
    await waitFor(async () =>
      expect((await findByRole('img')).getAttribute('src')).not.toBe(firstSrc),
    )
    expect(queryByText("Image didn't load")).toBeNull()
    expect(queryByText('Encrypted media — server could not decrypt')).toBeNull()
  })

  it('blames the moment, not encryption, when good bytes fail twice', async () => {
    // A PNG that will not paint is not a decryption failure and not a format
    // problem — asserting either is what cost a whole investigation.
    serveBytes()
    const { findByRole, findByText, queryByText } = renderImage(
      image({ encrypted: true }),
    )
    await failDecodeTwice(findByRole)
    expect(await findByText("Image didn't load")).toBeTruthy()
    expect(queryByText(/decrypt/)).toBeNull()
  })

  it('always offers retry, the only remedy a PWA user has', async () => {
    // No reload in a standalone PWA: without this button the reader's next
    // move is force-quitting the app, which is how this was found.
    serveBytes(CIPHERTEXT)
    const { findByRole, findByText, queryByText } = renderImage(
      image({ encrypted: true, mimetype: undefined, filename: 'blob' }),
    )
    const failedSrc = await failDecodeTwice(findByRole)
    await findByText('Could not display this image')

    // Retry re-fetches; serve a real image this time so the recovery is
    // visible rather than merely attempted.
    serveBytes()
    fireEvent.click(await findByRole('button', { name: 'Retry' }))

    // The url that just failed must never be re-mounted. `useMediaBlob` keeps
    // its previous `ready` state until its effect runs after paint, so
    // clearing the failure on the click would render the failed blob again —
    // and a browser re-fires `error` for it before the fresh bytes arrive,
    // relatching the placeholder over the image the retry fetched.
    expect(document.querySelector(`img[src="${failedSrc}"]`)).toBeNull()
    expect(queryByText('Could not display this image')).not.toBeNull()

    const recovered = await findByRole('img')
    expect(recovered.getAttribute('src')).not.toBe(failedSrc)
    expect(queryByText('Could not display this image')).toBeNull()
  })

  it("re-latches on the retry's own url, not the one before it", async () => {
    // The verdict is keyed to the bytes it was reached on, so a retry that
    // fails again is reported against *its* url — and Retry keeps working.
    serveBytes(CIPHERTEXT)
    const { findByRole, findByText } = renderImage(
      image({ mimetype: undefined, filename: 'blob' }),
    )
    const failedSrc = await failDecodeTwice(findByRole)
    fireEvent.click(await findByRole('button', { name: 'Retry' }))

    const second = await findByRole('img')
    expect(second.getAttribute('src')).not.toBe(failedSrc)
    fireEvent.error(second)
    expect(await findByText('Could not display this image')).toBeTruthy()
    expect(await findByRole('button', { name: 'Retry' })).toBeTruthy()
  })

  it('reports the server-confirmed decryption failure as fact', async () => {
    // The one time this client may say the words. It is not inferring it from
    // an `onError` — the server observed the failure and said so with
    // `422 media_undecryptable` (#361), which is the evidence #359 found the
    // client never had.
    serveBytes()
    const { findByRole, findByText, queryByRole } = renderImage(
      image({ encrypted: true }),
    )
    const img = await findByRole('img')
    server.use(
      http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
        HttpResponse.json(
          { error: { code: 'media_undecryptable', message: 'hash mismatch' } },
          { status: 422 },
        ),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
        () =>
          HttpResponse.json(
            {
              error: {
                code: 'media_undecryptable',
                message: 'hash mismatch',
              },
            },
            { status: 422 },
          ),
      ),
    )
    fireEvent.error(img)

    expect(
      await findByText('Encrypted media — the server could not decrypt it'),
    ).toBeTruthy()
    // No Retry: the same ciphertext would come back and fail identically, so
    // the button would promise something pressing it cannot deliver.
    expect(queryByRole('button', { name: 'Retry' })).toBeNull()
  })

  it('keeps retry for a 422 that is not a decryption failure', async () => {
    // The status is a general class. Reading the envelope `code` is what stops
    // some future unprocessable-entity from wearing the one message in this
    // client that sends people to go and look at their server.
    serveBytes()
    const { findByRole, findByText, queryByText } = renderImage(
      image({ encrypted: true }),
    )
    const img = await findByRole('img')
    server.use(
      http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
        HttpResponse.json(
          { error: { code: 'unprocessable', message: 'something else' } },
          { status: 422 },
        ),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
        () =>
          HttpResponse.json(
            { error: { code: 'unprocessable', message: 'something else' } },
            { status: 422 },
          ),
      ),
    )
    fireEvent.error(img)

    expect(await findByText('Could not load image')).toBeTruthy()
    expect(await findByRole('button', { name: 'Retry' })).toBeTruthy()
    expect(queryByText(/could not decrypt it/)).toBeNull()
  })

  it('offers retry when the fetch itself fails, not just the decode', async () => {
    // One decode glitch plus one network glitch must not rebuild the dead end
    // this change exists to remove.
    serveBytes()
    const { findByRole, findByText } = renderImage(image())
    // Let the first load land before breaking the route, so it is the
    // *automatic retry's* fetch that fails and not the original.
    const img = await findByRole('img')
    server.use(
      http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
        HttpResponse.json({ error: {} }, { status: 503 }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
        () => HttpResponse.json({ error: {} }, { status: 503 }),
      ),
    )
    fireEvent.error(img)
    expect(await findByText('Could not load image')).toBeTruthy()
    expect(await findByRole('button', { name: 'Retry' })).toBeTruthy()

    serveBytes()
    fireEvent.click(await findByRole('button', { name: 'Retry' }))
    expect(await findByRole('img')).toBeTruthy()
  })

  it('keeps download available for renderable bytes the browser fumbled', async () => {
    // The dead end in issue #359: a PNG's failure withheld Download, because
    // the old gate asked whether we could *name* an unrenderable format. These
    // bytes are a real file, so saving them works.
    serveBytes()
    const { findByRole, findByText } = renderImage(image())
    await failDecodeTwice(findByRole)
    await findByText("Image didn't load")
    expect(await findByRole('button', { name: 'Download' })).toBeTruthy()
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
