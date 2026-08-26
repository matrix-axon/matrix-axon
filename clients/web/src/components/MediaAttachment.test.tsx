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
})
afterAll(() => server.close())

function renderAttachment(media: ParsedMedia) {
  return render(
    <ServicesContext.Provider value={testServices()}>
      <MediaAttachment accountId={ACCOUNT} media={media} />
    </ServicesContext.Provider>,
  )
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

    fireEvent.click(getByRole('button', { name: 'Download' }))
    await waitFor(() => expect(click).toHaveBeenCalledOnce())
  })

  it('offers no Download button when the media has no mxc url', () => {
    const { queryByRole } = renderAttachment(file({ url: null }))
    expect(queryByRole('button', { name: 'Download' })).toBeNull()
  })

  it('surfaces a failure when the download errors', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:media`, () =>
        HttpResponse.json({ error: {} }, { status: 404 }),
      ),
    )
    const { getByRole, findByText } = renderAttachment(file())
    fireEvent.click(getByRole('button', { name: 'Download' }))
    expect(await findByText('Download failed')).toBeTruthy()
  })
})
