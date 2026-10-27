import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { options } from 'preact'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, expect, it, vi } from 'vitest'
import { MAX_PREVIEW_IMAGE_BYTES } from '../link-previews'
import { ServicesContext } from '../services'
import type { TimelineEvent } from '../stores/timeline'
import { encryptedPreviewFile } from '../test/link-preview'
import { TEST_BASE_URL, testServices } from '../test/services'
import { EventBody } from './EventBody'

const URL = 'https://example.org/article'
const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
const server = setupServer()
const frameScheduler = options.requestAnimationFrame
beforeAll(() => {
  options.requestAnimationFrame = queueMicrotask
  server.listen({ onUnhandledRequest: 'error' })
})
afterEach(() => {
  cleanup()
  server.resetHandlers()
  vi.restoreAllMocks()
})
afterAll(async () => {
  await Promise.resolve()
  options.requestAnimationFrame = frameScheduler
  server.close()
})

function event(fields: Record<string, unknown> = {}): TimelineEvent {
  return {
    account_id: 'first',
    room_id: '!room:hs',
    event_id: '$image-preview',
    sender: '@a:hs',
    type: 'm.room.message',
    state_key: null,
    body: URL,
    origin_ts: 1,
    arrival_order: 1,
    redacted: false,
    content: {
      msgtype: 'm.text',
      'm.url_previews': [
        {
          'matrix:matched_url': URL,
          'og:title': 'Article',
          'og:image': 'mxc://hs/preview',
          ...fields,
        },
      ],
    },
  } as unknown as TimelineEvent
}

function view(
  initial = event(),
  configure?: (services: ReturnType<typeof testServices>) => void,
) {
  const services = testServices({ token: 'first-token' })
  const acquire = vi.spyOn(services.media, 'acquire')
  configure?.(services)
  const element = (value: TimelineEvent, compact = false) => (
    <ServicesContext.Provider value={services}>
      <EventBody event={value} showLinkPreviews={!compact} />
    </ServicesContext.Provider>
  )
  const rendered = render(element(initial))
  return {
    ...rendered,
    services,
    acquire,
    update: (value: TimelineEvent, compact = false) =>
      rendered.rerender(element(value, compact)),
  }
}

function serve(thumbnailStatus = 200, originalStatus = 200) {
  const requests: string[] = []
  server.use(
    http.get(`${TEST_BASE_URL}/v1/media/*`, ({ request }) => {
      requests.push(request.url)
      expect(request.headers.get('authorization')).toMatch(
        /^Bearer (first|second)-token$/,
      )
      return new HttpResponse(PNG, {
        status: request.url.includes('/thumbnail')
          ? thumbnailStatus
          : originalStatus,
        headers: { 'content-type': 'image/png' },
      })
    }),
  )
  return requests
}
const ready = (container: Element) =>
  waitFor(() =>
    expect(
      container.querySelector('.link-preview-image img')?.getAttribute('src'),
    ).toMatch(/^blob:/),
  )

it('requests bounded authenticated plain thumbnails and encrypted originals', async () => {
  const requests = serve()
  const rendered = view()
  await ready(rendered.container)
  expect(requests[0]).toContain('/thumbnail?width=480&height=270&method=scale')
  expect(rendered.acquire).toHaveBeenLastCalledWith(
    'first',
    'mxc://hs/preview',
    {
      thumbnail: { width: 480, height: 270, method: 'scale' },
      contentType: undefined,
      maxBytes: MAX_PREVIEW_IMAGE_BYTES,
    },
  )
  rendered.update(event({ 'matrix:image:encrypted': encryptedPreviewFile }))
  // A new descriptor cannot paint the previous object's blob for one frame.
  expect(rendered.container.querySelector('img')).toBeNull()
  await ready(rendered.container)
  expect(requests[1]).toBe(
    `${TEST_BASE_URL}/v1/media/first/hs/encrypted-preview`,
  )
})

it('falls back to an original once, then retains the text and safe destination', async () => {
  const requests = serve(404, 404)
  const rendered = view()
  await waitFor(() =>
    expect(rendered.getByText('Preview image unavailable')).toBeTruthy(),
  )
  expect(requests).toHaveLength(2)
  expect(
    rendered.container.querySelector<HTMLAnchorElement>('.link-preview-card')!
      .href,
  ).toBe(URL)
  expect(rendered.getByText('Article')).toBeTruthy()
})

it('bounds decode recovery and releases handles on redaction and late completion', async () => {
  const releases: ReturnType<typeof vi.fn>[] = []
  let complete!: () => void
  const rendered = view(event(), (services) => {
    vi.mocked(services.media.acquire).mockImplementation(
      () =>
        new Promise((resolve) => {
          const release = vi.fn()
          releases.push(release)
          complete = () =>
            resolve({
              result: { ok: true, url: `blob:preview-${releases.length}` },
              release,
            })
        }),
    )
  })
  await waitFor(() => expect(complete).toBeDefined())
  complete()
  await ready(rendered.container)
  fireEvent.error(rendered.container.querySelector('img')!)
  await waitFor(() => expect(releases).toHaveLength(2))
  expect(releases[0]).toHaveBeenCalledOnce()
  complete()
  await ready(rendered.container)
  fireEvent.error(rendered.container.querySelector('img')!)
  await waitFor(() =>
    expect(rendered.getByText('Preview image unavailable')).toBeTruthy(),
  )
  expect(releases[1]).toHaveBeenCalledOnce()
  expect(rendered.acquire).toHaveBeenCalledTimes(2)
  rendered.update(event({ 'og:image': 'mxc://hs/next' }))
  await waitFor(() => expect(releases).toHaveLength(3))
  rendered.update({ ...event(), redacted: true })
  complete()
  await waitFor(() => expect(releases[2]).toHaveBeenCalledOnce())
  expect(rendered.container.querySelector('.link-previews')).toBeNull()
})

it('does not fetch for hostile images, spoilers, compact roots or removed metadata', async () => {
  const rendered = view(event({ 'og:image': 'https://tracking.test/pixel' }))
  await waitFor(() => expect(rendered.getByText('Article')).toBeTruthy())
  rendered.update({
    ...event(),
    content: {
      ...event().content,
      format: 'org.matrix.custom.html',
      formatted_body: `<span data-mx-spoiler>${URL}</span>`,
    },
  } as unknown as TimelineEvent)
  rendered.update(event(), true)
  rendered.update({
    ...event(),
    content: { msgtype: 'm.text' },
  } as unknown as TimelineEvent)
  expect(rendered.acquire).not.toHaveBeenCalled()
})

it('releases an edited image, isolates accounts and reacquires after a token swap', async () => {
  const requests = serve()
  const rendered = view()
  await ready(rendered.container)
  const first = rendered.container.querySelector('img')!.src
  rendered.update(event({ 'og:image': 'mxc://hs/edited' }))
  expect(rendered.container.querySelector('img')).toBeNull()
  await ready(rendered.container)
  expect(rendered.container.querySelector('img')!.src).not.toBe(first)
  rendered.update({ ...event(), account_id: 'second' })
  expect(rendered.container.querySelector('img')).toBeNull()
  await ready(rendered.container)
  const previous = rendered.container.querySelector('img')!.src
  rendered.services.auth.tokenPaste.setToken('second-token')
  await ready(rendered.container)
  await waitFor(() =>
    expect(rendered.container.querySelector('img')!.src).not.toBe(previous),
  )
  expect(requests).toHaveLength(4)
  rendered.services.auth.finishSignOut()
  await waitFor(() =>
    expect(rendered.container.querySelector('img')).toBeNull(),
  )
})

it.each([
  [413, 'payload_too_large'],
  [422, 'media_undecryptable'],
])(
  'keeps terminal media failure %s as text without original retries',
  async (status, code) => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/media/*`, () =>
        HttpResponse.json(
          {
            error: { code, message: 'Unavailable' },
          },
          { status },
        ),
      ),
    )
    const rendered = view()
    await waitFor(() =>
      expect(rendered.getByText('Preview image unavailable')).toBeTruthy(),
    )
    expect(rendered.getByText('Article')).toBeTruthy()
    expect(rendered.acquire).toHaveBeenCalledOnce()
  },
)

it('releases the previous token result when it arrives after the replacement session', async () => {
  const releases = [vi.fn(), vi.fn()]
  const finishes: (() => void)[] = []
  const rendered = view(event(), (services) => {
    vi.mocked(services.media.acquire).mockImplementation(
      () =>
        new Promise((resolve) => {
          const index = finishes.length
          finishes.push(() =>
            resolve({
              result: { ok: true, url: `blob:reader-${index}` },
              release: releases[index]!,
            }),
          )
        }),
    )
  })
  await waitFor(() => expect(finishes).toHaveLength(1))
  rendered.services.auth.tokenPaste.setToken('second-token')
  await waitFor(() => expect(finishes).toHaveLength(2))
  finishes[0]!()
  await waitFor(() => expect(releases[0]).toHaveBeenCalledOnce())
  expect(rendered.container.querySelector('img')).toBeNull()
  finishes[1]!()
  await ready(rendered.container)
  expect(rendered.container.querySelector('img')!.getAttribute('src')).toBe(
    'blob:reader-1',
  )
})

it('renders a successful original after a failed plain thumbnail request', async () => {
  const requests = serve(404, 200)
  const rendered = view()
  await ready(rendered.container)
  expect(requests).toHaveLength(2)
  expect(
    rendered.container.querySelector('.link-preview-image')!.textContent,
  ).not.toContain('unavailable')
})
