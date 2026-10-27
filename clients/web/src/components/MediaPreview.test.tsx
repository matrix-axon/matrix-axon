import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import type { ParsedMedia } from '../media/parse-media'
import { ServicesContext } from '../services'
import { TEST_BASE_URL, testServices } from '../test/services'
import { MediaAttachment } from './MediaAttachment'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'

function media(overrides: Partial<ParsedMedia> = {}): ParsedMedia {
  return {
    kind: 'file',
    url: 'mxc://hs/doc',
    thumbnailUrl: null,
    filename: 'report.pdf',
    caption: null,
    encrypted: false,
    mimetype: 'application/pdf',
    size: 1536,
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

function serveMedia(body: string, contentType: string) {
  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/media/:account/:server/:media`,
      () =>
        new HttpResponse(body, { headers: { 'content-type': contentType } }),
    ),
  )
}

function renderAttachment(parsed: ParsedMedia) {
  return render(
    <ServicesContext.Provider value={testServices()}>
      <MediaAttachment accountId={ACCOUNT} media={parsed} />
    </ServicesContext.Provider>,
  )
}

describe('attachment media preview', () => {
  it('fetches nothing until the user expands', () => {
    // No msw handler is registered, and `onUnhandledRequest: 'error'` would
    // fail the test if the card fetched on mount.
    const { getByRole } = renderAttachment(media())
    expect(getByRole('button', { name: 'Open report.pdf' })).toBeTruthy()
  })

  it('opens a pdf in the lightbox, drawn rather than framed', async () => {
    serveMedia('%PDF-1.4', 'application/octet-stream')
    const { getByRole } = renderAttachment(media())

    fireEvent.click(getByRole('button', { name: 'Open report.pdf' }))
    await waitFor(() =>
      expect(document.querySelector('.lightbox .pdf-viewer')).toBeTruthy(),
    )
    // Never an <iframe>: iOS Safari draws only page 1 of a framed PDF, so a
    // multi-page document was unreadable on a phone (ADR 0072).
    expect(document.querySelector('.lightbox iframe')).toBeNull()
  })

  it('renders a text attachment as escaped text', async () => {
    serveMedia('<b>not markup</b>', 'application/octet-stream')
    const { getByRole, findByText } = renderAttachment(
      media({ filename: 'notes.txt', mimetype: 'text/plain' }),
    )

    fireEvent.click(getByRole('button', { name: 'notes.txt' }))
    const code = await findByText('<b>not markup</b>')
    // A `<code>` inside the `<pre>` since ADR 0073 — the text is still a Preact
    // text child, so the markup in it stays inert.
    expect(code.tagName).toBe('CODE')
    expect(code.closest('pre')).toBeTruthy()
    expect(code.querySelector('b')).toBeNull()
  })

  it('syntax-highlights a source file from its extension', async () => {
    serveMedia('def f():\n    return 1\n', 'application/octet-stream')
    const { getByRole } = renderAttachment(
      media({ filename: 'deploy.py', mimetype: 'text/plain' }),
    )

    fireEvent.click(getByRole('button', { name: 'deploy.py' }))
    await waitFor(() =>
      expect(getByRole('dialog').querySelector('.hljs-keyword')).toBeTruthy(),
    )
    // The text is intact — highlighting decorates, it does not rewrite.
    expect(getByRole('dialog').querySelector('pre')!.textContent).toBe(
      'def f():\n    return 1\n',
    )
  })

  it('highlights an extensionless script from its shebang', async () => {
    serveMedia(
      '#!/usr/bin/env python3\nimport os\n',
      'application/octet-stream',
    )
    const { getByRole } = renderAttachment(
      media({ filename: 'deploy', mimetype: 'text/plain' }),
    )

    fireEvent.click(getByRole('button', { name: 'deploy' }))
    await waitFor(() =>
      expect(getByRole('dialog').querySelector('.hljs-keyword')).toBeTruthy(),
    )
  })

  it('shows plain text for a language it cannot identify', async () => {
    serveMedia('just some notes\n', 'application/octet-stream')
    const { getByRole, findByText } = renderAttachment(
      media({ filename: 'notes.txt', mimetype: 'text/plain' }),
    )

    fireEvent.click(getByRole('button', { name: 'notes.txt' }))
    expect(await findByText('just some notes')).toBeTruthy()
    expect(getByRole('dialog').querySelector('.hljs')).toBeNull()
  })

  it('opens a video in the lightbox', async () => {
    serveMedia('ftyp', 'video/mp4')
    const { getByRole } = renderAttachment(
      media({ kind: 'video', filename: 'clip.mp4', mimetype: 'video/mp4' }),
    )

    fireEvent.click(getByRole('button', { name: 'Play clip.mp4' }))
    await waitFor(() =>
      expect(document.querySelector('.lightbox video')).toBeTruthy(),
    )
  })

  it('opens audio in the shared preview dialog and releases it on close', async () => {
    serveMedia('ID3', 'audio/mpeg')
    const { getByRole } = renderAttachment(
      media({ kind: 'audio', filename: 'voice.mp3', mimetype: 'audio/mpeg' }),
    )
    fireEvent.click(getByRole('button', { name: 'voice.mp3' }))
    await waitFor(() =>
      expect(getByRole('dialog').querySelector('audio')).toBeTruthy(),
    )
    fireEvent.click(getByRole('button', { name: 'Close' }))
    await waitFor(() => expect(document.querySelector('.lightbox')).toBeNull())
  })

  it('closes the lightbox with Escape', async () => {
    serveMedia('%PDF-1.4', 'application/octet-stream')
    const { getByRole } = renderAttachment(media())

    fireEvent.click(getByRole('button', { name: 'Open report.pdf' }))
    await waitFor(() =>
      expect(document.querySelector('.lightbox')).toBeTruthy(),
    )
    fireEvent.keyDown(document, { key: 'Escape' })
    await waitFor(() => expect(document.querySelector('.lightbox')).toBeNull())
  })

  it('closes the lightbox on a tap beside the media', async () => {
    serveMedia('ftyp', 'video/mp4')
    const { getByRole } = renderAttachment(
      media({ kind: 'video', filename: 'clip.mp4', mimetype: 'video/mp4' }),
    )

    fireEvent.click(getByRole('button', { name: 'Play clip.mp4' }))
    await waitFor(() =>
      expect(document.querySelector('.lightbox video')).toBeTruthy(),
    )
    // The letterbox area around a video belongs to the figure, not the
    // overlay — a target/currentTarget check would ignore this tap.
    fireEvent.click(document.querySelector('.lightbox-figure')!)
    await waitFor(() => expect(document.querySelector('.lightbox')).toBeNull())
  })

  it('does not close when the media itself is clicked', async () => {
    serveMedia('ftyp', 'video/mp4')
    const { getByRole } = renderAttachment(
      media({ kind: 'video', filename: 'clip.mp4', mimetype: 'video/mp4' }),
    )

    fireEvent.click(getByRole('button', { name: 'Play clip.mp4' }))
    await waitFor(() =>
      expect(document.querySelector('.lightbox video')).toBeTruthy(),
    )
    // Clicks land on a <video>'s own controls; closing the thing being
    // scrubbed would be maddening.
    fireEvent.click(document.querySelector('.lightbox video')!)
    expect(document.querySelector('.lightbox')).toBeTruthy()
  })

  it('closes the lightbox with the close button', async () => {
    serveMedia('%PDF-1.4', 'application/octet-stream')
    const { getByRole } = renderAttachment(media())

    fireEvent.click(getByRole('button', { name: 'Open report.pdf' }))
    await waitFor(() =>
      expect(document.querySelector('.lightbox')).toBeTruthy(),
    )
    fireEvent.click(getByRole('button', { name: 'Close' }))
    await waitFor(() => expect(document.querySelector('.lightbox')).toBeNull())
  })

  it('leaves the expand button re-openable after closing', async () => {
    serveMedia('%PDF-1.4', 'application/octet-stream')
    const { getByRole } = renderAttachment(media())

    fireEvent.click(getByRole('button', { name: 'Open report.pdf' }))
    await waitFor(() =>
      expect(document.querySelector('.lightbox')).toBeTruthy(),
    )
    fireEvent.click(getByRole('button', { name: 'Close' }))
    await waitFor(() => expect(document.querySelector('.lightbox')).toBeNull())
    // Never "Hide": the lightbox is dismissed from inside, so the card's
    // button always reads as the way in.
    fireEvent.click(getByRole('button', { name: 'Open report.pdf' }))
    await waitFor(() =>
      expect(document.querySelector('.lightbox')).toBeTruthy(),
    )
  })

  it('reports a failed preview without losing the Download button', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
        HttpResponse.json({ error: {} }, { status: 404 }),
      ),
    )
    const { getByRole, findByText } = renderAttachment(media())

    fireEvent.click(getByRole('button', { name: 'Open report.pdf' }))
    expect(await findByText('Could not load pdf')).toBeTruthy()
    expect(
      getByRole('dialog').querySelector('[aria-label="Download report.pdf"]'),
    ).toBeTruthy()
  })

  it('offers no preview for a kind with no inline renderer', () => {
    const { queryByRole } = renderAttachment(
      media({ filename: 'archive.zip', mimetype: 'application/zip' }),
    )
    expect(queryByRole('button', { name: 'Preview' })).toBeNull()
    expect(queryByRole('button', { name: 'Play' })).toBeNull()
  })
})
