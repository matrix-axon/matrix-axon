import { fireEvent, render, waitFor } from '@testing-library/preact'
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
import type { Platform } from '../platform'
import { ServicesContext } from '../services'
import type { TimelineEvent } from '../stores/timeline'
import { TEST_BASE_URL, testServices } from '../test/services'
import { EventBody } from './EventBody'
import { MediaAttachment } from './MediaAttachment'
import { STALL_PROBE_MS } from './StreamPreview'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'
const MINT = `${TEST_BASE_URL}/v1/media/:account/:server/:media/stream-ticket`
const STREAM = `${TEST_BASE_URL}/v1/media-stream/:ticket`
const BLOB = `${TEST_BASE_URL}/v1/media/:account/:server/:media`

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  server.resetHandlers()
  vi.restoreAllMocks()
})
afterAll(() => server.close())

function video(overrides: Partial<ParsedMedia> = {}): ParsedMedia {
  return {
    kind: 'video',
    url: 'mxc://hs/clip',
    thumbnailUrl: null,
    filename: 'clip.mp4',
    caption: null,
    encrypted: false,
    mimetype: 'video/mp4',
    size: 1536,
    ...overrides,
  }
}

/**
 * Issue tickets `t1`, `t2`, … in order, recording each mint request's path.
 * `status` answers every mint with that error instead.
 */
function serveTickets(status?: number): string[] {
  const minted: string[] = []
  server.use(
    http.post(MINT, ({ request }) => {
      minted.push(new URL(request.url).pathname)
      if (status !== undefined) {
        return HttpResponse.json(
          { error: { code: 'bad_request', message: 'no' } },
          { status },
        )
      }
      return HttpResponse.json({
        data: {
          path: `/v1/media-stream/t${minted.length}`,
          expires_at: '2099-01-01T00:00:00Z',
        },
      })
    }),
  )
  return minted
}

/** Answer the error probe on a ticket URL with `status`, recording its Range. */
function serveProbe(status: number): string[] {
  const ranges: string[] = []
  server.use(
    http.get(STREAM, ({ request }) => {
      ranges.push(request.headers.get('range') ?? '')
      return new HttpResponse(status < 300 ? 'x' : null, { status })
    }),
  )
  return ranges
}

function renderAttachment(parsed: ParsedMedia, platform?: Partial<Platform>) {
  return render(
    <ServicesContext.Provider
      value={testServices(platform === undefined ? {} : { platform })}
    >
      <MediaAttachment accountId={ACCOUNT} media={parsed} />
    </ServicesContext.Provider>,
  )
}

/** Open the attachment's preview and wait for its player. */
async function openPlayer(
  view: ReturnType<typeof renderAttachment>,
  selector = '.lightbox video',
): Promise<HTMLMediaElement> {
  const name = /^(Play|Preview) |\.mp3$|\.ogg$/
  fireEvent.click(view.getAllByRole('button', { name })[0]!)
  return waitFor(() => {
    const el = document.querySelector<HTMLMediaElement>(selector)
    expect(el).toBeTruthy()
    return el!
  })
}

/** Make the element report a failure, as a browser would. */
function failElement(el: HTMLMediaElement, code: number) {
  Object.defineProperty(el, 'error', {
    configurable: true,
    value: { code },
  })
  fireEvent.error(el)
}

const MEDIA_ERR_NETWORK = 2
const MEDIA_ERR_SRC_NOT_SUPPORTED = 4

describe('stream-ticket playback (ADR 0139 § 6)', () => {
  it('plays a video from its ticket URL, not a blob', async () => {
    const minted = serveTickets()
    const el = await openPlayer(renderAttachment(video()))
    expect(el.getAttribute('src')).toBe(`${TEST_BASE_URL}/v1/media-stream/t1`)
    // No handler serves `GET /v1/media/...`, and an unhandled request fails
    // the test: the object itself never went through the blob path.
    expect(minted).toEqual([`/v1/media/${ACCOUNT}/hs/clip/stream-ticket`])
  })

  it('plays audio from its ticket URL', async () => {
    serveTickets()
    const el = await openPlayer(
      renderAttachment(
        video({
          kind: 'audio',
          url: 'mxc://hs/voice',
          filename: 'voice.mp3',
          mimetype: 'audio/mpeg',
        }),
      ),
      '.lightbox audio',
    )
    expect(el.getAttribute('src')).toBe(`${TEST_BASE_URL}/v1/media-stream/t1`)
  })

  it('offers a video over the blob ceiling, since nothing buffers it', async () => {
    serveTickets()
    const el = await openPlayer(
      renderAttachment(video({ size: 1024 * 1024 * 1024 })),
    )
    expect(el.getAttribute('src')).toBe(`${TEST_BASE_URL}/v1/media-stream/t1`)
  })

  it('keeps the blob path in a shell that cannot load ticket URLs', async () => {
    // No mint handler: a ticket request would fail the test.
    server.use(
      http.get(
        BLOB,
        () =>
          new HttpResponse('ftyp', {
            headers: { 'content-type': 'video/mp4' },
          }),
      ),
    )
    const el = await openPlayer(
      renderAttachment(video(), { mediaStreamSrc: null }),
    )
    expect(el.getAttribute('src')).toMatch(/^blob:/)
  })

  it('keeps a shell at the blob ceiling', () => {
    const view = renderAttachment(video({ size: 1024 * 1024 * 1024 }), {
      mediaStreamSrc: null,
    })
    expect(view.queryByRole('button', { name: 'Play clip.mp4' })).toBeNull()
  })

  it('falls back to the blob path when the server refuses a ticket', async () => {
    serveTickets(400)
    server.use(
      http.get(
        BLOB,
        () =>
          new HttpResponse('ftyp', {
            headers: { 'content-type': 'video/mp4' },
          }),
      ),
    )
    const el = await openPlayer(renderAttachment(video()))
    expect(el.getAttribute('src')).toMatch(/^blob:/)
  })

  it('does not buffer a refused object larger than the blob ceiling', async () => {
    serveTickets(400)
    const view = renderAttachment(video({ size: 1024 * 1024 * 1024 }))
    fireEvent.click(view.getByRole('button', { name: 'Play clip.mp4' }))
    expect(
      await view.findByText(/clip\.mp4 is too large to play here/),
    ).toBeTruthy()
  })

  it('offers Retry when no ticket can be issued, and retries', async () => {
    serveTickets(503)
    const view = renderAttachment(video())
    fireEvent.click(view.getByRole('button', { name: 'Play clip.mp4' }))
    const retry = await view.findByRole('button', { name: 'Retry' })

    server.resetHandlers()
    serveTickets()
    fireEvent.click(retry)
    await waitFor(() =>
      expect(
        document.querySelector('.lightbox video')?.getAttribute('src'),
      ).toBe(`${TEST_BASE_URL}/v1/media-stream/t1`),
    )
  })

  describe('when the element reports an error', () => {
    it('blames the codec when the probe reaches the bytes', async () => {
      serveTickets()
      const ranges = serveProbe(206)
      const view = renderAttachment(video())
      const el = await openPlayer(view)

      failElement(el, MEDIA_ERR_SRC_NOT_SUPPORTED)
      expect(
        await view.findByText(/This browser cannot play clip\.mp4/),
      ).toBeTruthy()
      expect(ranges).toEqual(['bytes=0-0'])
    })

    it('offers Retry, not a codec message, for a dropped connection', async () => {
      serveTickets()
      serveProbe(206)
      const view = renderAttachment(video())
      const el = await openPlayer(view)

      failElement(el, MEDIA_ERR_NETWORK)
      expect(await view.findByRole('button', { name: 'Retry' })).toBeTruthy()
      expect(view.queryByText(/This browser cannot play/)).toBeNull()
    })

    it('reports an undecryptable attachment without Retry', async () => {
      serveTickets()
      serveProbe(422)
      const view = renderAttachment(video())
      const el = await openPlayer(view)

      failElement(el, MEDIA_ERR_SRC_NOT_SUPPORTED)
      expect(
        await view.findByText('Could not decrypt this video.'),
      ).toBeTruthy()
      expect(view.queryByRole('button', { name: 'Retry' })).toBeNull()
    })

    it('offers Retry for any other probe answer', async () => {
      serveTickets()
      serveProbe(503)
      const view = renderAttachment(video())
      const el = await openPlayer(view)

      failElement(el, MEDIA_ERR_SRC_NOT_SUPPORTED)
      expect(await view.findByRole('button', { name: 'Retry' })).toBeTruthy()
    })

    for (const status of [401, 403, 404]) {
      it(`replaces an expired ticket (${status}) and resumes where it stopped`, async () => {
        const minted = serveTickets()
        serveProbe(status)
        const play = vi.spyOn(HTMLMediaElement.prototype, 'play')
        const el = await openPlayer(renderAttachment(video()))
        Object.defineProperty(el, 'currentTime', {
          configurable: true,
          writable: true,
          value: 42,
        })
        fireEvent.play(el)

        failElement(el, MEDIA_ERR_SRC_NOT_SUPPORTED)
        await waitFor(() =>
          expect(el.getAttribute('src')).toBe(
            `${TEST_BASE_URL}/v1/media-stream/t2`,
          ),
        )
        expect(minted).toHaveLength(2)

        // The new load starts from zero; its metadata puts the playhead back.
        el.currentTime = 0
        fireEvent.loadedMetadata(el)
        expect(el.currentTime).toBe(42)
        expect(play).toHaveBeenCalled()
      })
    }

    it('replaces a ticket only once until the new one loads', async () => {
      const minted = serveTickets()
      serveProbe(404)
      const view = renderAttachment(video())
      const el = await openPlayer(view)

      failElement(el, MEDIA_ERR_SRC_NOT_SUPPORTED)
      await waitFor(() => expect(el.getAttribute('src')).toMatch(/\/t2$/))
      // The replacement fails before loading anything: give up, not loop.
      failElement(el, MEDIA_ERR_SRC_NOT_SUPPORTED)
      expect(await view.findByRole('button', { name: 'Retry' })).toBeTruthy()
      expect(minted).toHaveLength(2)
    })

    it('recovers again once the replacement has loaded', async () => {
      // An OAuth access token lives an hour, so a long video outlives several
      // tickets; each one that loads earns the next replacement.
      const minted = serveTickets()
      serveProbe(404)
      const el = await openPlayer(renderAttachment(video()))

      failElement(el, MEDIA_ERR_SRC_NOT_SUPPORTED)
      await waitFor(() => expect(el.getAttribute('src')).toMatch(/\/t2$/))
      fireEvent.loadedData(el)
      failElement(el, MEDIA_ERR_SRC_NOT_SUPPORTED)
      await waitFor(() => expect(el.getAttribute('src')).toMatch(/\/t3$/))
      expect(minted).toHaveLength(3)
    })
  })

  describe('when the player stalls instead (WebKit/AVFoundation)', () => {
    // AVFoundation never raises `error` for a refused range mid-playback; it
    // raises `stalled` and re-requests the range in a tight loop.
    const advance = async (ms: number) => {
      await vi.advanceTimersByTimeAsync(ms)
    }

    it('replaces an expired ticket, which ends the request loop', async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true })
      try {
        const minted = serveTickets()
        const ranges = serveProbe(404)
        const el = await openPlayer(renderAttachment(video()))

        fireEvent.stalled(el)
        await advance(STALL_PROBE_MS.stalled + 10)
        await waitFor(() => expect(el.getAttribute('src')).toMatch(/\/t2$/))
        expect(ranges).toEqual(['bytes=0-0'])
        expect(minted).toHaveLength(2)
      } finally {
        vi.useRealTimers()
      }
    })

    it('probes a stall while playback still runs on its buffer', async () => {
      // What a real iPhone did after a server restart: the playhead kept
      // moving on buffered data while AVFoundation looped on the dead
      // ticket, and nothing but `stalled` said so.
      vi.useFakeTimers({ shouldAdvanceTime: true })
      try {
        serveTickets()
        const ranges = serveProbe(404)
        const el = await openPlayer(renderAttachment(video()))
        let now = 10
        Object.defineProperty(el, 'currentTime', {
          configurable: true,
          get: () => now,
          set: (v: number) => (now = v),
        })

        fireEvent.stalled(el)
        now = 11
        await advance(STALL_PROBE_MS.stalled + 10)
        await waitFor(() => expect(el.getAttribute('src')).toMatch(/\/t2$/))
        expect(ranges).toEqual(['bytes=0-0'])
      } finally {
        vi.useRealTimers()
      }
    })

    it('does not probe a stall that data arrives after', async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true })
      try {
        serveTickets()
        const ranges = serveProbe(404)
        const el = await openPlayer(renderAttachment(video()))

        fireEvent.stalled(el)
        fireEvent.progress(el)
        await advance(STALL_PROBE_MS.stalled + 10)
        expect(ranges).toEqual([])
      } finally {
        vi.useRealTimers()
      }
    })

    it('also probes a long wait with no stalled event', async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true })
      try {
        serveTickets()
        const ranges = serveProbe(404)
        const el = await openPlayer(renderAttachment(video()))

        fireEvent.waiting(el)
        await advance(STALL_PROBE_MS.waiting - 100)
        expect(ranges).toEqual([])
        await advance(200)
        await waitFor(() => expect(el.getAttribute('src')).toMatch(/\/t2$/))
      } finally {
        vi.useRealTimers()
      }
    })

    it('probes a stuck seek even while the engine claims enough data', async () => {
      // AVFoundation keeps readyState at HAVE_ENOUGH_DATA (4) through a seek
      // whose ranges are refused.
      vi.useFakeTimers({ shouldAdvanceTime: true })
      try {
        serveTickets()
        serveProbe(404)
        const el = await openPlayer(renderAttachment(video()))
        Object.defineProperty(el, 'readyState', {
          configurable: true,
          value: 4,
        })
        Object.defineProperty(el, 'paused', { configurable: true, value: true })
        Object.defineProperty(el, 'seeking', {
          configurable: true,
          value: true,
        })

        fireEvent.waiting(el)
        await advance(STALL_PROBE_MS.waiting + 10)
        await waitFor(() => expect(el.getAttribute('src')).toMatch(/\/t2$/))
      } finally {
        vi.useRealTimers()
      }
    })

    it('does not probe a wait that a paused player has what it needs for', async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true })
      try {
        serveTickets()
        const ranges = serveProbe(404)
        const el = await openPlayer(renderAttachment(video()))
        Object.defineProperty(el, 'readyState', {
          configurable: true,
          value: 4,
        })
        Object.defineProperty(el, 'paused', { configurable: true, value: true })

        fireEvent.waiting(el)
        await advance(STALL_PROBE_MS.waiting + 10)
        expect(ranges).toEqual([])
      } finally {
        vi.useRealTimers()
      }
    })

    it('does not probe a wait that playback moves on from', async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true })
      try {
        serveTickets()
        const ranges = serveProbe(404)
        const el = await openPlayer(renderAttachment(video()))
        let now = 0
        Object.defineProperty(el, 'currentTime', {
          configurable: true,
          get: () => now,
          set: (v: number) => (now = v),
        })

        fireEvent.waiting(el)
        now = 3
        await advance(STALL_PROBE_MS.waiting + 10)
        expect(ranges).toEqual([])
      } finally {
        vi.useRealTimers()
      }
    })

    it('leaves a slow but reachable stream alone, probing at most every ten seconds', async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true })
      try {
        const minted = serveTickets()
        const ranges = serveProbe(206)
        const view = renderAttachment(video())
        const el = await openPlayer(view)

        fireEvent.stalled(el)
        await advance(STALL_PROBE_MS.stalled + 10)
        await waitFor(() => expect(ranges).toHaveLength(1))
        // A slow link keeps raising `stalled`; the next one is too soon.
        fireEvent.stalled(el)
        await advance(STALL_PROBE_MS.stalled + 10)
        expect(ranges).toHaveLength(1)
        await advance(STALL_PROBE_MS.minInterval)
        fireEvent.stalled(el)
        await advance(STALL_PROBE_MS.stalled + 10)
        await waitFor(() => expect(ranges).toHaveLength(2))
        expect(el.getAttribute('src')).toMatch(/\/t1$/)
        expect(minted).toHaveLength(1)
        expect(view.queryByText(/cannot play/)).toBeNull()
      } finally {
        vi.useRealTimers()
      }
    })

    it('lets no stall timer armed earlier undo a final state', async () => {
      // Review on PR 1190: a `waiting` armed the 5 s timer, an `error` then
      // settled on "could not decrypt", and the old timer fired into the
      // closure of the render that armed it, probed again and revived the
      // player on a new ticket.
      vi.useFakeTimers({ shouldAdvanceTime: true })
      try {
        const minted = serveTickets()
        const answers = [422, 404]
        const ranges: string[] = []
        server.use(
          http.get(STREAM, ({ request }) => {
            ranges.push(request.headers.get('range') ?? '')
            return new HttpResponse(null, { status: answers.shift() ?? 404 })
          }),
        )
        const view = renderAttachment(video())
        const el = await openPlayer(view)

        fireEvent.waiting(el)
        await advance(2_000)
        failElement(el, MEDIA_ERR_SRC_NOT_SUPPORTED)
        expect(
          await view.findByText('Could not decrypt this video.'),
        ).toBeTruthy()

        await advance(STALL_PROBE_MS.waiting)
        expect(ranges).toEqual(['bytes=0-0'])
        expect(minted).toHaveLength(1)
        expect(view.getByText('Could not decrypt this video.')).toBeTruthy()
        expect(document.querySelector('.lightbox video')).toBeNull()
      } finally {
        vi.useRealTimers()
      }
    })

    it('reports an undecryptable attachment, removing the looping player', async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true })
      try {
        serveTickets()
        serveProbe(422)
        const view = renderAttachment(video())
        const el = await openPlayer(view)

        fireEvent.stalled(el)
        await advance(STALL_PROBE_MS.stalled + 10)
        expect(
          await view.findByText('Could not decrypt this video.'),
        ).toBeTruthy()
        expect(document.querySelector('.lightbox video')).toBeNull()
      } finally {
        vi.useRealTimers()
      }
    })
  })

  it('stops the download when the preview closes', async () => {
    serveTickets()
    const load = vi.spyOn(HTMLMediaElement.prototype, 'load')
    const view = renderAttachment(video())
    const el = await openPlayer(view)

    fireEvent.click(view.getByRole('button', { name: 'Close' }))
    await waitFor(() => expect(document.querySelector('.lightbox')).toBeNull())
    expect(el.hasAttribute('src')).toBe(false)
    expect(load).toHaveBeenCalled()
  })
})

function mediaEvent(id: string, msgtype: 'm.video' | 'm.audio'): TimelineEvent {
  const ext = msgtype === 'm.video' ? 'mp4' : 'mp3'
  return {
    account_id: ACCOUNT,
    event_id: id,
    room_id: '!r:hs',
    sender: '@alice:hs',
    origin_ts: 1,
    arrival_order: 1,
    type: 'm.room.message',
    body: `${id}.${ext}`,
    content: {
      msgtype,
      body: `${id}.${ext}`,
      url: `mxc://hs/${id}`,
      info: {
        mimetype: msgtype === 'm.video' ? 'video/mp4' : 'audio/mpeg',
        size: 200 * 1024 * 1024,
      },
    },
    redacted: false,
    edited: false,
    edit_count: 0,
    state_key: null,
    reactions: null,
    relates_to: null,
  } as unknown as TimelineEvent
}

describe('a rendered timeline (ADR 0139 § 6)', () => {
  it('issues no ticket and starts no download for media nobody opened', async () => {
    const minted = serveTickets()
    const fetched: string[] = []
    server.use(
      http.get(`${TEST_BASE_URL}/v1/*`, ({ request }) => {
        fetched.push(new URL(request.url).pathname)
        return new HttpResponse(null, { status: 404 })
      }),
    )
    const events = [
      mediaEvent('v1', 'm.video'),
      mediaEvent('a1', 'm.audio'),
      mediaEvent('v2', 'm.video'),
    ]
    const view = render(
      <ServicesContext.Provider value={testServices()}>
        <ol>
          {events.map((event) => (
            <li key={event.event_id}>
              <EventBody event={event} />
            </li>
          ))}
        </ol>
      </ServicesContext.Provider>,
    )
    // Rendered as playable cards, so the absence of tickets is not just the
    // absence of a preview.
    await view.findByRole('button', { name: 'Play v1.mp4' })
    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(minted).toEqual([])
    expect(fetched.filter((path) => !path.endsWith('/thumbnail'))).toEqual([])
  })
})
