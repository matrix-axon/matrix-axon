import { cleanup, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import type { ParsedMedia } from '../media/parse-media'
import { ServicesContext } from '../services'
import { TEST_BASE_URL, testServices } from '../test/services'
import { MediaAttachment } from './MediaAttachment'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'

function video(overrides: Partial<ParsedMedia> = {}): ParsedMedia {
  return {
    kind: 'video',
    url: 'mxc://hs/clip',
    thumbnailUrl: null,
    filename: 'clip.mp4',
    caption: null,
    encrypted: false,
    mimetype: 'video/mp4',
    size: 2_360_866,
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

function renderAttachment(parsed: ParsedMedia) {
  return render(
    <ServicesContext.Provider value={testServices()}>
      <MediaAttachment accountId={ACCOUNT} media={parsed} />
    </ServicesContext.Provider>,
  )
}

describe('MediaTile', () => {
  it('names the tile by what clicking it does', () => {
    const { getByRole } = renderAttachment(video())
    expect(getByRole('button', { name: 'Play clip.mp4' })).toBeTruthy()

    cleanup()
    const pdf = renderAttachment(
      video({
        kind: 'file',
        filename: 'report.pdf',
        mimetype: 'application/pdf',
      }),
    )
    expect(pdf.getByRole('button', { name: 'Open report.pdf' })).toBeTruthy()
  })

  it('fetches nothing when there is no sender thumbnail', () => {
    // `onUnhandledRequest: 'error'` fails the test on any request, and the
    // tile is a placeholder — it must not download the object to draw itself.
    renderAttachment(video())
  })

  it('reserves the video shape so a resolving poster cannot shift the timeline', () => {
    const { container } = renderAttachment(video({ w: 1920, h: 1080 }))
    const surface = container.querySelector<HTMLElement>('.media-tile-surface')
    expect(surface!.style.aspectRatio).toBe('1920 / 1080')
  })

  it('falls back to a 16:9 box when the sender declared no dimensions', () => {
    const { container } = renderAttachment(video())
    const surface = container.querySelector<HTMLElement>('.media-tile-surface')
    expect(surface!.style.aspectRatio).toBe('16 / 9')
  })

  it('shows the duration when the sender declared one', () => {
    const { getByText } = renderAttachment(video({ duration: 187_000 }))
    expect(getByText('3:07')).toBeTruthy()
  })

  it('renders markdown in the tile caption', () => {
    const { container } = renderAttachment(
      video({ caption: 'a **bold** caption' }),
    )
    const caption = container.querySelector('.media-attachment-caption')
    expect(caption?.querySelector('strong')?.textContent).toBe('bold')
    expect(caption?.textContent).toBe('a bold caption')
  })

  it('shows a sender-supplied thumbnail as the poster', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
        () =>
          new HttpResponse(new Uint8Array([1, 2, 3]), {
            headers: { 'content-type': 'image/jpeg' },
          }),
      ),
    )
    const { container } = renderAttachment(
      video({ thumbnailUrl: 'mxc://hs/poster' }),
    )
    await waitFor(() =>
      expect(container.querySelector('.media-tile-poster img')).toBeTruthy(),
    )
  })

  it('keeps the filename, size and Download on the tile', () => {
    const { getByText, getByRole } = renderAttachment(video())
    expect(getByText('clip.mp4')).toBeTruthy()
    expect(getByText('2.3 MB')).toBeTruthy()
    expect(getByRole('button', { name: 'Download' })).toBeTruthy()
  })

  it('leaves audio and other files as a row card', () => {
    const { container } = renderAttachment(
      video({ kind: 'audio', filename: 'voice.mp3', mimetype: 'audio/mpeg' }),
    )
    expect(container.querySelector('.media-tile')).toBeNull()
    expect(container.querySelector('.media-attachment')).toBeTruthy()
  })
})
