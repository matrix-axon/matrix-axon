import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  it,
  vi,
} from 'vitest'
import type { ParsedMedia } from '../media/parse-media'
import { ServicesContext } from '../services'
import { TEST_BASE_URL, testServices } from '../test/services'
import { MediaAttachment } from './MediaAttachment'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'
const BYTES = new Uint8Array([1, 2, 3, 4])

function file(overrides: Partial<ParsedMedia> = {}): ParsedMedia {
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
  window.getSelection()?.removeAllRanges()
  vi.restoreAllMocks()
})
afterAll(() => server.close())

function renderAttachment(media: ParsedMedia, content?: unknown) {
  const services = testServices()
  return {
    ...render(
      <ServicesContext.Provider value={services}>
        <MediaAttachment accountId={ACCOUNT} media={media} content={content} />
      </ServicesContext.Provider>,
    ),
    services,
  }
}

describe('MediaAttachment', () => {
  it('renders the filename and a human-readable size', () => {
    const { getByText } = renderAttachment(file())
    expect(getByText('report.pdf')).toBeTruthy()
    expect(getByText('1.5 KB')).toBeTruthy()
  })

  it('renders a distinct body as the attachment caption', () => {
    const { getByText } = renderAttachment(file({ caption: 'Seems bad' }))
    expect(getByText('report.pdf')).toBeTruthy()
    expect(getByText('Seems bad')).toBeTruthy()
  })

  it('renders markdown in the attachment caption', () => {
    const { container } = renderAttachment(
      file({ caption: 'a **bold** caption' }),
    )
    const caption = container.querySelector('.media-attachment-caption')
    expect(caption?.querySelector('strong')?.textContent).toBe('bold')
    expect(caption?.textContent).toBe('a bold caption')
  })

  it('downloads via a transient anchor on Download click', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media`,
        () =>
          new HttpResponse(BYTES, {
            headers: { 'content-type': 'application/pdf' },
          }),
      ),
    )
    const click = vi.spyOn(HTMLAnchorElement.prototype, 'click')
    const { getByRole } = renderAttachment(file())

    fireEvent.click(getByRole('button', { name: 'Download report.pdf' }))
    await waitFor(() => expect(click).toHaveBeenCalledOnce())
    expect(document.querySelector('[role="dialog"]')).toBeNull()
  })

  it('offers no Download button when the media has no mxc url', () => {
    const { queryByRole } = renderAttachment(file({ url: null }))
    expect(queryByRole('button', { name: 'Download report.pdf' })).toBeNull()
  })

  it('surfaces a failure when the download errors', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
        HttpResponse.json({ error: {} }, { status: 404 }),
      ),
    )
    const { getByRole, findByText } = renderAttachment(file())
    fireEvent.click(getByRole('button', { name: 'Download report.pdf' }))
    expect(await findByText('Download failed')).toBeTruthy()
  })
})

it('opens the preview from caption text but preserves embedded links and spoilers', async () => {
  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/media/:account/:server/:media`,
      () => new HttpResponse('Preview text'),
    ),
  )
  const view = renderAttachment(
    file({
      filename: 'notes.txt',
      mimetype: 'text/plain',
      caption: 'Caption https://example.org/ hidden',
    }),
    {
      format: 'org.matrix.custom.html',
      formatted_body:
        'Caption <a href="https://example.org/">site</a> <span data-mx-spoiler>hidden</span>',
    },
  )
  const fetchText = vi.spyOn(view.services.media, 'fetchText')
  const caption = view.container.querySelector<HTMLElement>(
    '.media-attachment-caption',
  )!
  fireEvent.click(caption.querySelector('a')!)
  expect(document.querySelector('.lightbox')).toBeNull()
  const spoiler = caption.querySelector<HTMLElement>('.spoiler')!
  fireEvent.click(spoiler)
  expect(spoiler.classList.contains('spoiler-revealed')).toBe(true)
  expect(document.querySelector('.lightbox')).toBeNull()
  expect(fetchText).not.toHaveBeenCalled()
  fireEvent.click(caption, { detail: 1 })
  expect(await view.findByText('Preview text')).toBeTruthy()
  expect(view.getByRole('dialog')).toBeTruthy()
  expect(fetchText).toHaveBeenCalledOnce()
  fireEvent.click(view.getByRole('dialog').querySelector('code')!)
  expect(view.getByRole('dialog')).toBeTruthy()
})

it('keeps selected caption text from opening the viewer', () => {
  const view = renderAttachment(
    file({
      filename: 'notes.txt',
      mimetype: 'text/plain',
      caption: 'Select this caption',
    }),
  )
  const caption = view.container.querySelector('.media-attachment-caption')!
  const range = document.createRange()
  range.selectNodeContents(caption)
  window.getSelection()!.addRange(range)
  fireEvent.click(caption, { detail: 1 })
  expect(document.querySelector('.lightbox')).toBeNull()
})

it('opens with an unrelated selection elsewhere on the page', () => {
  const view = renderAttachment(
    file({ filename: 'notes.txt', mimetype: 'text/plain' }),
  )
  vi.spyOn(view.services.media, 'fetchText').mockResolvedValue({
    ok: true,
    text: 'Preview text',
  })
  const elsewhere = document.createElement('p')
  elsewhere.textContent = 'Another message'
  document.body.append(elsewhere)
  const range = document.createRange()
  range.selectNodeContents(elsewhere)
  window.getSelection()!.addRange(range)
  fireEvent.click(view.container.querySelector('.media-attachment')!, {
    detail: 1,
  })
  expect(view.queryByRole('dialog')).toBeTruthy()
  elsewhere.remove()
})

it('clears a previous download failure when opening a fresh viewer', async () => {
  const view = renderAttachment(
    file({ filename: 'notes.txt', mimetype: 'text/plain' }),
  )
  server.use(
    http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
      HttpResponse.json({ error: {} }, { status: 404 }),
    ),
  )
  fireEvent.click(view.getByRole('button', { name: /Download/ }))
  expect(await view.findByText('Download failed')).toBeTruthy()
  vi.spyOn(view.services.media, 'fetchText').mockResolvedValue({
    ok: true,
    text: 'Preview text',
  })
  fireEvent.click(view.container.querySelector('.media-attachment')!)
  expect(view.queryByText('Download failed')).toBeNull()
  fireEvent.click(view.getByRole('button', { name: 'Close' }))
  expect(view.queryByText('Download failed')).toBeNull()
})

it('keeps the text preview wrapper from dismissing the dialog', async () => {
  const view = renderAttachment(
    file({ filename: 'notes.txt', mimetype: 'text/plain' }),
  )
  vi.spyOn(view.services.media, 'fetchText').mockResolvedValue({
    ok: true,
    text: 'Preview text',
  })
  fireEvent.click(view.container.querySelector('.media-attachment')!)
  await view.findByText('Preview text')
  fireEvent.click(
    view.getByRole('dialog').querySelector('.media-text-preview')!,
  )
  expect(view.queryByRole('dialog')).toBeTruthy()
})

it.each([
  { filename: 'archive.zip', mimetype: 'application/zip' },
  { filename: 'large.txt', mimetype: 'text/plain', size: 512 * 1024 + 1 },
])('keeps unsupported/oversized $filename download-only', (descriptor) => {
  const view = renderAttachment(file(descriptor))
  const card = view.container.querySelector('.media-attachment')!
  expect(card.hasAttribute('data-media-preview-card')).toBe(false)
  fireEvent.click(card)
  expect(document.querySelector('.lightbox')).toBeNull()
  expect(
    view.getByRole('button', { name: `Download ${descriptor.filename}` }),
  ).toBeTruthy()
  expect(view.container.querySelector('[aria-haspopup="dialog"]')).toBeNull()
})
