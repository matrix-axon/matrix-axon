import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import {
  afterAll,
  afterEach,
  beforeAll,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from 'vitest'
import { signal } from '@preact/signals'
import { createApiClient } from '../api/client'
import { probeAudio, probeVideo } from '../media/media-probe'
import { createMediaService } from '../media/media-service'
import type { MediaSendSupport, MediaUploadKind } from './capabilities'
import { createTimelineStore, type EventDto } from './timeline'

// jsdom decodes no media; `media-probe.test.ts` covers the probe itself.
vi.mock('../media/media-probe', () => ({
  probeVideo: vi.fn(),
  probeAudio: vi.fn(),
}))

/**
 * The media-send path (ADR 0065): stage the bytes, claim them with
 * `send-media`, and reconcile the optimistic echo — the same echo machinery a
 * text send uses, so these tests lean on it rather than re-proving it.
 */

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const UPLOAD_ID = '22222222-2222-4222-8222-222222222222'
const UPLOAD_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/media/uploads`
const SEND_MEDIA_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send-media`
const EVENTS_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

let revoked: string[]
beforeEach(() => {
  revoked = []
  vi.spyOn(URL, 'createObjectURL').mockImplementation(() => 'blob:preview')
  vi.spyOn(URL, 'revokeObjectURL').mockImplementation((url) => {
    revoked.push(url)
  })
})
afterEach(() => vi.restoreAllMocks())

function makeStore(threadRoot?: string, support?: MediaSendSupport) {
  const auth = {
    getToken: () => 'tok-test',
    onAuthFailure: () => {},
    LoginBootstrap: () => null,
  }
  const api = createApiClient(auth, BASE_URL)
  const media = createMediaService({ auth, baseUrl: BASE_URL })
  return createTimelineStore(api, media, ACCOUNT, ROOM, threadRoot, support)
}

function confirmed(id: string, body: string): EventDto {
  return {
    account_id: ACCOUNT,
    event_id: id,
    room_id: ROOM,
    sender: '@me:hs',
    origin_ts: 100,
    arrival_order: 100,
    type: 'm.room.message',
    body,
    content: { msgtype: 'm.image', body, url: 'mxc://hs/abc' },
    redacted: false,
    edited: false,
    edit_count: 0,
  } as unknown as EventDto
}

/** The happy path: both endpoints succeed, the confirmed event is fetchable. */
function serveSuccess(): { sendBody: () => unknown } {
  let sendBody: unknown
  server.use(
    http.post(UPLOAD_PATH, () =>
      HttpResponse.json({ data: { upload_id: UPLOAD_ID } }),
    ),
    http.post(SEND_MEDIA_PATH, async ({ request }) => {
      sendBody = await request.json()
      return HttpResponse.json({ data: { event_id: '$img' } })
    }),
    http.get(EVENTS_PATH, () =>
      HttpResponse.json({ data: confirmed('$img', 'a caption') }),
    ),
  )
  return { sendBody: () => sendBody }
}

const png = () => new File(['bytes'], 'cat.png', { type: 'image/png' })

it('sends formatted staged captions with routing, previews, and durable progress', async () => {
  const store = makeStore('$thread')
  const progress = vi.fn(async () => {})
  const formatted = '<img data-mx-emoticon src="mxc://hs/custom" alt=":cat:">'
  let body: unknown
  let progressAtRequest: unknown
  server.use(
    http.post(SEND_MEDIA_PATH, async ({ request }) => {
      body = await request.json()
      progressAtRequest = [...progress.mock.calls]
      return HttpResponse.json({ data: { event_id: '$staged' } })
    }),
    http.get(EVENTS_PATH, () =>
      HttpResponse.json({ data: confirmed('$staged', ':cat:') }),
    ),
  )
  expect(
    await store.sendStagedMedia(UPLOAD_ID, ':cat:', {
      formattedBody: formatted,
      replyTo: '$reply',
      urlPreviews: [],
      progress,
    }),
  ).toBe(true)
  expect(body).toEqual({
    upload_id: UPLOAD_ID,
    caption: ':cat:',
    format: 'org.matrix.custom.html',
    formatted_body: formatted,
    reply_to: '$reply',
    thread_root: '$thread',
    url_previews: [],
  })
  expect(progressAtRequest).toEqual([['dispatching']])
  expect(progress.mock.calls).toEqual([['dispatching'], ['sent', '$staged']])
})

it('retains an inline formatted caption through media failure and retry even when the caption equals the filename', async () => {
  const store = makeStore()
  const bodies: Record<string, unknown>[] = []
  const formatted = '<img data-mx-emoticon src="mxc://hs/custom" alt=":cat:">'
  server.use(
    http.post(UPLOAD_PATH, () =>
      HttpResponse.json({ data: { upload_id: UPLOAD_ID } }),
    ),
    http.post(SEND_MEDIA_PATH, async ({ request }) => {
      bodies.push((await request.json()) as Record<string, unknown>)
      return bodies.length === 1
        ? HttpResponse.json(
            { error: { code: 'upstream', message: 'Unavailable' } },
            { status: 502 },
          )
        : HttpResponse.json({ data: { event_id: '$caption' } })
    }),
    http.get(EVENTS_PATH, () =>
      HttpResponse.json({ data: confirmed('$caption', 'cat.png') }),
    ),
  )
  expect(
    await store.sendMedia(png(), {
      caption: 'cat.png',
      formattedBody: formatted,
    }),
  ).toBe(false)
  const echo = store.events.value.find(
    (event) => event.localEcho?.status === 'failed',
  )!
  expect((echo.content as { formatted_body?: string }).formatted_body).toBe(
    formatted,
  )
  expect(await store.retrySend(echo.event_id)).toBe(true)
  expect(bodies).toHaveLength(2)
  for (const body of bodies) {
    expect(body.caption).toBe('cat.png')
    expect(body.format).toBe('org.matrix.custom.html')
    expect(body.formatted_body).toBe(formatted)
  }
})

describe('sendMedia', () => {
  it('echoes the image locally, then claims the staged upload and reconciles', async () => {
    const seen = serveSuccess()
    const store = makeStore()

    const result = store.sendMedia(png(), {
      caption: 'a caption',
      senderId: '@me:hs',
    })

    // The echo renders synchronously — the whole point of not waiting out an
    // upload's transfer time in silence.
    expect(store.events.value).toHaveLength(1)
    const echo = store.events.value[0]
    expect(echo.localEcho?.status).toBe('pending')
    expect(echo.localEcho?.media?.previewUrl).toBe('blob:preview')
    expect(echo.localEcho?.media?.kind).toBe('image')
    expect(echo.body).toBe('a caption')

    expect(await result).toBe(true)

    expect(seen.sendBody()).toEqual({
      upload_id: UPLOAD_ID,
      caption: 'a caption',
      reply_to: null,
      thread_root: null,
      // Sent to any server; only one with `send_media_thumbnails` uses it.
      txn_id: expect.stringMatching(/^[A-Za-z0-9._~-]{1,64}$/),
    })
    // The echo is gone, replaced by the server's event — and its preview url
    // with it, since the confirmed event renders from the media proxy.
    expect(store.events.value).toHaveLength(1)
    expect(store.events.value[0].event_id).toBe('$img')
    expect(store.events.value[0].localEcho).toBeUndefined()
    expect(revoked).toEqual(['blob:preview'])
  })

  it('uses the filename as the echo body when there is no caption', async () => {
    serveSuccess()
    const store = makeStore()

    const result = store.sendMedia(png())

    // The server falls back to the filename for the event body, so the echo
    // must too — that identity is what lets the live frame confirm the echo.
    expect(store.events.value[0].body).toBe('cat.png')
    await result
  })

  it('carries reply and thread relations exactly as a text send does', async () => {
    const seen = serveSuccess()
    const store = makeStore('$root')

    await store.sendMedia(png(), { replyTo: '$parent' })

    expect(seen.sendBody()).toMatchObject({
      reply_to: '$parent',
      thread_root: '$root',
    })
  })

  it('stages a non-image as an m.file echo with no preview', async () => {
    serveSuccess()
    const store = makeStore()

    const result = store.sendMedia(
      new File(['%PDF'], 'notes.pdf', { type: 'application/pdf' }),
    )

    const echo = store.events.value[0]
    expect(echo.localEcho?.media?.kind).toBe('file')
    expect(echo.localEcho?.media?.previewUrl).toBeNull()
    expect((echo.content as unknown as { msgtype: string }).msgtype).toBe(
      'm.file',
    )
    await result
  })

  it('keeps the file on a failed upload, and retries with the same bytes', async () => {
    let uploads = 0
    server.use(
      http.post(UPLOAD_PATH, () => {
        uploads += 1
        return uploads === 1
          ? HttpResponse.json(
              { error: { code: 'bad_gateway', message: 'homeserver down' } },
              { status: 502 },
            )
          : HttpResponse.json({ data: { upload_id: UPLOAD_ID } })
      }),
      http.post(SEND_MEDIA_PATH, () =>
        HttpResponse.json({ data: { event_id: '$img' } }),
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({ data: confirmed('$img', 'cat.png') }),
      ),
    )
    const store = makeStore()

    expect(await store.sendMedia(png())).toBe(false)

    // A failed upload must not cost the user the file they picked.
    const failed = store.events.value[0]
    expect(failed.localEcho?.status).toBe('failed')
    expect(failed.localEcho?.media?.file.name).toBe('cat.png')
    expect(store.error.value).toBe('Upload failed')

    expect(await store.retrySend(failed.event_id)).toBe(true)

    expect(uploads).toBe(2)
    expect(store.events.value).toHaveLength(1)
    expect(store.events.value[0].event_id).toBe('$img')
  })

  it('marks the echo failed when the send-media mutation is rejected', async () => {
    server.use(
      http.post(UPLOAD_PATH, () =>
        HttpResponse.json({ data: { upload_id: UPLOAD_ID } }),
      ),
      http.post(SEND_MEDIA_PATH, () =>
        HttpResponse.json(
          { error: { code: 'not_found', message: 'unknown upload' } },
          { status: 404 },
        ),
      ),
    )
    const store = makeStore()

    expect(await store.sendMedia(png())).toBe(false)

    expect(store.events.value[0].localEcho?.status).toBe('failed')
    expect(store.error.value).toBe('unknown upload')
    // Retained for the retry — the staged upload is re-made from the file.
    expect(store.events.value[0].localEcho?.media?.file).toBeInstanceOf(File)
  })

  it('revokes the preview url when a failed echo is discarded', async () => {
    server.use(
      http.post(UPLOAD_PATH, () =>
        HttpResponse.json(
          { error: { code: 'bad_request', message: 'nope' } },
          { status: 400 },
        ),
      ),
    )
    const store = makeStore()
    await store.sendMedia(png())

    store.discardSend(store.events.value[0].event_id)

    expect(store.events.value).toHaveLength(0)
    expect(revoked).toEqual(['blob:preview'])
  })

  it('drops the echo — and its preview — when a live frame confirms the send first', async () => {
    server.use(
      http.post(UPLOAD_PATH, () =>
        HttpResponse.json({ data: { upload_id: UPLOAD_ID } }),
      ),
      http.post(SEND_MEDIA_PATH, () => {
        // The bus beats our own reconcile: the frame lands while the mutation's
        // response is still in flight (WCR-07).
        store.ingestLive(confirmed('$img', 'cat.png'))
        return HttpResponse.json({ data: { event_id: '$img' } })
      }),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({ data: confirmed('$img', 'cat.png') }),
      ),
    )
    const store = makeStore()

    expect(await store.sendMedia(png(), { senderId: '@me:hs' })).toBe(true)

    // Exactly once in the slice, not twice.
    expect(store.events.value).toHaveLength(1)
    expect(store.events.value[0].event_id).toBe('$img')
    expect(revoked).toEqual(['blob:preview'])
  })
})

/**
 * Multi-image send (ADR 0081). Matrix carries no album marker (#130), so a
 * batch is N independent events whose order the *server* assigns at send time
 * — which is why the uploads run sequentially rather than in parallel.
 */
describe('sendMediaBatch', () => {
  const files = (...names: string[]) =>
    names.map((name) => new File(['bytes'], name, { type: 'image/png' }))

  /** Records send order, and lets each send be resolved on demand. */
  function serveBatch(): { sends: () => unknown[] } {
    const sends: unknown[] = []
    let n = 0
    server.use(
      http.post(UPLOAD_PATH, () =>
        HttpResponse.json({ data: { upload_id: UPLOAD_ID } }),
      ),
      http.post(SEND_MEDIA_PATH, async ({ request }) => {
        const body = await request.json()
        sends.push(body)
        n += 1
        return HttpResponse.json({ data: { event_id: `$img${n}` } })
      }),
      // Body echoes the id, so slice order is readable in assertions.
      http.get(EVENTS_PATH, ({ params }) =>
        HttpResponse.json({
          data: confirmed(String(params.eventId), String(params.eventId)),
        }),
      ),
    )
    return { sends: () => sends }
  }

  it('pushes every echo before any bytes move, so the gallery appears at once', async () => {
    serveBatch()
    const store = makeStore()
    const pending = store.sendMediaBatch(files('a.png', 'b.png', 'c.png'))

    // Synchronously, before awaiting: all three are already in the slice.
    expect(store.events.value).toHaveLength(3)
    expect(
      store.events.value.every((e) => e.localEcho?.status === 'pending'),
    ).toBe(true)
    await pending
  })

  it('uploads sequentially, preserving the order the user picked', async () => {
    // Not a simplification: the server assigns room order at send time, so
    // parallel uploads would shuffle the batch.
    const { sends } = serveBatch()
    const store = makeStore()
    await store.sendMediaBatch(files('a.png', 'b.png', 'c.png'))

    expect(sends()).toHaveLength(3)
    // Order preserved, and the slice reflects it.
    expect(store.events.value.map((e) => e.event_id)).toEqual([
      '$img1',
      '$img2',
      '$img3',
    ])
  })

  it('captions and replies to the first image only', async () => {
    // Thread membership is a destination, not a decoration — but N reply
    // blocks would look wrong and break the gallery run (ADR 0081).
    const { sends } = serveBatch()
    const store = makeStore()
    await store.sendMediaBatch(files('a.png', 'b.png'), {
      caption: 'look at these',
      formattedBody:
        'look <img data-mx-emoticon src="mxc://hs/custom" alt=":custom:">',
      replyTo: '$parent',
    })

    const bodies = sends() as {
      caption: unknown
      reply_to: unknown
      formatted_body?: string
    }[]
    expect(bodies[0].caption).toBe('look at these')
    expect(bodies[0].reply_to).toBe('$parent')
    expect(bodies[0].formatted_body).toContain('mxc://hs/custom')
    expect(bodies[1].caption).toBeNull()
    expect(bodies[1].reply_to).toBeNull()
    expect(bodies[1].formatted_body).toBeUndefined()
  })

  it('applies the thread root to every image', async () => {
    const { sends } = serveBatch()
    const store = makeStore('$root')
    await store.sendMediaBatch(files('a.png', 'b.png'))

    const roots = sends().map(
      (b) => (b as { thread_root: unknown }).thread_root,
    )
    expect(roots).toEqual(['$root', '$root'])
  })

  it('marks each echo with its position, for display only', async () => {
    serveBatch()
    const store = makeStore()
    const pending = store.sendMediaBatch(files('a.png', 'b.png', 'c.png'))

    expect(store.events.value.map((e) => e.localEcho?.batch)).toEqual([
      { index: 0, total: 3 },
      { index: 1, total: 3 },
      { index: 2, total: 3 },
    ])
    await pending
  })

  it('leaves a single send unmarked, so a retry cannot claim a batch', async () => {
    serveBatch()
    const store = makeStore()
    const pending = store.sendMediaBatch(files('only.png'))
    expect(store.events.value[0].localEcho?.batch).toBeUndefined()
    await pending
  })

  it('continues past a failure and reports how much of the batch it covers', async () => {
    let sends = 0
    server.use(
      http.post(UPLOAD_PATH, () =>
        HttpResponse.json({ data: { upload_id: UPLOAD_ID } }),
      ),
      http.post(SEND_MEDIA_PATH, () => {
        sends += 1
        // The middle one fails; the others must still go.
        return sends === 2
          ? HttpResponse.json({ error: { message: 'nope' } }, { status: 500 })
          : HttpResponse.json({ data: { event_id: `$img${sends}` } })
      }),
      http.get(EVENTS_PATH, ({ params }) =>
        HttpResponse.json({ data: confirmed(String(params.eventId), 'x') }),
      ),
    )
    const store = makeStore()
    const ok = await store.sendMediaBatch(files('a.png', 'b.png', 'c.png'))

    expect(ok).toBe(false)
    expect(sends).toBe(3)
    // One error slot, so it says how much of the batch failed.
    expect(store.error.value).toContain('1 of 3')
    const failed = store.events.value.filter(
      (e) => e.localEcho?.status === 'failed',
    )
    expect(failed).toHaveLength(1)
  })

  it('keeps the batch in room order when a live frame confirms one mid-flight', async () => {
    // The hazard ADR 0081 flagged: a WS frame confirming image 1 can beat its
    // own reconcile, and `ingestLive` appends at the tail — which would put
    // image 1 *after* the rest.
    serveBatch()
    const store = makeStore()
    // The sender must match, or the frame reads as a new event rather than a
    // confirmation and the race never happens.
    const pending = store.sendMediaBatch(files('a.png', 'b.png', 'c.png'), {
      senderId: '@me:hs',
    })
    const firstEchoBody = store.events.value[0].localEcho!.body

    store.ingestLive(confirmed('$live1', firstEchoBody))
    await pending

    // Image 1 must stay first, however it was confirmed.
    const order = store.events.value.map((e) => e.body)
    expect(order).toHaveLength(3)
    expect(order[0]).toBe(firstEchoBody)
  })
})

/**
 * Video and audio (ADR 0142 §§ 1, 4 and 7, issue 24): their own kinds, and
 * a probe's `info` and poster, only against a server that lists them.
 */
describe('video and audio sends', () => {
  const POSTER_ID = '33333333-3333-4333-8333-333333333333'
  const mov = () =>
    new File(['moov'], 'IMG_9306.mov', { type: 'video/quicktime' })
  const m4a = () => new File(['ftyp'], 'memo.m4a', { type: 'audio/mp4' })
  const poster = () => new Blob(['jpeg'], { type: 'image/jpeg' })
  const VIDEO_INFO = { width: 1920, height: 1080, duration_ms: 31251 }

  function supporting(
    kinds: readonly MediaUploadKind[] | null = [
      'image',
      'file',
      'video',
      'audio',
    ],
    thumbnails: boolean | null = true,
  ): MediaSendSupport {
    return {
      mediaUploadKinds: signal(kinds),
      sendMediaThumbnails: signal(thumbnails),
    }
  }

  /** Every staging request's kind and type, and every send-media body. */
  function serve(
    options: {
      posterStatus?: number
      sendStatus?: () => number
    } = {},
  ) {
    const staged: { kind: string | null; type: string | null }[] = []
    const sends: Record<string, unknown>[] = []
    server.use(
      http.post(UPLOAD_PATH, ({ request }) => {
        const url = new URL(request.url)
        const kind = url.searchParams.get('kind')
        staged.push({ kind, type: request.headers.get('content-type') })
        if (url.searchParams.get('filename') === 'poster.jpg') {
          return options.posterStatus === undefined
            ? HttpResponse.json({ data: { upload_id: POSTER_ID } })
            : HttpResponse.json(
                { error: { code: 'x', message: 'no' } },
                { status: options.posterStatus },
              )
        }
        return HttpResponse.json({ data: { upload_id: UPLOAD_ID } })
      }),
      http.post(SEND_MEDIA_PATH, async ({ request }) => {
        sends.push((await request.json()) as Record<string, unknown>)
        const status = options.sendStatus?.() ?? 200
        return status === 200
          ? HttpResponse.json({ data: { event_id: '$vid' } })
          : HttpResponse.json(
              { error: { code: 'gateway_timeout', message: 'slow' } },
              { status },
            )
      }),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({ data: confirmed('$vid', 'IMG_9306.mov') }),
      ),
    )
    return { staged, sends }
  }

  beforeEach(() => {
    vi.mocked(probeVideo).mockReset()
    vi.mocked(probeAudio).mockReset()
    vi.mocked(probeVideo).mockResolvedValue({
      info: VIDEO_INFO,
      poster: poster(),
    })
    vi.mocked(probeAudio).mockResolvedValue({
      info: { duration_ms: 4200 },
      poster: null,
    })
  })

  it('sends a video as m.video with its info, poster and txn_id', async () => {
    const seen = serve()
    const store = makeStore(undefined, supporting())

    const result = store.sendMedia(mov())
    const echo = store.events.value[0]
    expect(echo.localEcho?.media?.kind).toBe('video')
    expect((echo.content as unknown as { msgtype: string }).msgtype).toBe(
      'm.video',
    )
    expect(await result).toBe(true)

    expect(probeVideo).toHaveBeenCalledTimes(1)
    expect(seen.staged).toEqual(
      expect.arrayContaining([
        { kind: 'video', type: 'video/quicktime' },
        { kind: 'image', type: 'image/jpeg' },
      ]),
    )
    expect(seen.sends).toEqual([
      {
        upload_id: UPLOAD_ID,
        caption: null,
        reply_to: null,
        thread_root: null,
        info: VIDEO_INFO,
        thumbnail_upload_id: POSTER_ID,
        txn_id: expect.any(String),
      },
    ])
  })

  it('sends audio as m.audio with its duration and no thumbnail', async () => {
    const seen = serve()
    const store = makeStore(undefined, supporting())

    expect(await store.sendMedia(m4a())).toBe(true)

    expect(probeVideo).not.toHaveBeenCalled()
    expect(seen.staged).toEqual([{ kind: 'audio', type: 'audio/mp4' }])
    expect(seen.sends[0]).toMatchObject({ info: { duration_ms: 4200 } })
    expect(seen.sends[0]).not.toHaveProperty('thumbnail_upload_id')
  })

  it('stages video as a file, and probes nothing, against an older server', async () => {
    const seen = serve()
    // A server without `media_upload_kinds` or `send_media_thumbnails`.
    const store = makeStore(undefined, supporting(['image', 'file'], false))

    expect(await store.sendMedia(mov())).toBe(true)

    expect(store.events.value[0].localEcho).toBeUndefined()
    expect(probeVideo).not.toHaveBeenCalled()
    expect(seen.staged).toEqual([{ kind: 'file', type: 'video/quicktime' }])
    expect(seen.sends[0]).not.toHaveProperty('info')
    expect(seen.sends[0]).not.toHaveProperty('thumbnail_upload_id')
  })

  it('treats capabilities not yet known as an older server', async () => {
    const seen = serve()
    const store = makeStore(undefined, supporting(null, null))

    await store.sendMedia(mov())

    expect(probeVideo).not.toHaveBeenCalled()
    expect(seen.staged).toEqual([{ kind: 'file', type: 'video/quicktime' }])
  })

  it('uses the new kinds without a poster when the server lists only them', async () => {
    const seen = serve()
    const store = makeStore(undefined, supporting(undefined, false))

    await store.sendMedia(mov())

    expect(probeVideo).not.toHaveBeenCalled()
    expect(seen.staged).toEqual([{ kind: 'video', type: 'video/quicktime' }])
    expect(seen.sends[0]).not.toHaveProperty('info')
  })

  it('sends without info or poster when the probe fails', async () => {
    vi.mocked(probeVideo).mockResolvedValue(null)
    const seen = serve()
    const store = makeStore(undefined, supporting())

    expect(await store.sendMedia(mov())).toBe(true)

    expect(seen.staged).toEqual([{ kind: 'video', type: 'video/quicktime' }])
    expect(seen.sends[0]).not.toHaveProperty('info')
    expect(seen.sends[0]).not.toHaveProperty('thumbnail_upload_id')
  })

  it('keeps the info when only the poster upload fails', async () => {
    const seen = serve({ posterStatus: 502 })
    const store = makeStore(undefined, supporting())

    expect(await store.sendMedia(mov())).toBe(true)

    expect(seen.sends[0]).toMatchObject({ info: VIDEO_INFO })
    expect(seen.sends[0]).not.toHaveProperty('thumbnail_upload_id')
  })

  it('retries with the same txn_id, info and poster, probing once', async () => {
    let attempt = 0
    const seen = serve({ sendStatus: () => (++attempt === 1 ? 504 : 200) })
    const store = makeStore(undefined, supporting())

    expect(await store.sendMedia(mov())).toBe(false)
    const failed = store.events.value[0]
    expect(failed.localEcho?.status).toBe('failed')

    expect(await store.retrySend(failed.event_id)).toBe(true)

    expect(probeVideo).toHaveBeenCalledTimes(1)
    expect(seen.sends).toHaveLength(2)
    const [first, second] = seen.sends
    expect(second.txn_id).toBe(first.txn_id)
    expect(second.info).toEqual(first.info)
    expect(second.thumbnail_upload_id).toBe(POSTER_ID)
    // The poster is staged again: the first one went with the failed send.
    expect(seen.staged.filter((s) => s.kind === 'image')).toHaveLength(2)
  })

  it('does not add a thumbnail on retry that the first request lacked', async () => {
    let attempt = 0
    let posterStatus: number | undefined = 502
    const seen = serve({
      sendStatus: () => {
        attempt += 1
        posterStatus = undefined
        return attempt === 1 ? 504 : 200
      },
      get posterStatus() {
        return posterStatus
      },
    })
    const store = makeStore(undefined, supporting())

    expect(await store.sendMedia(mov())).toBe(false)
    expect(await store.retrySend(store.events.value[0].event_id)).toBe(true)

    // The server answers a retry whose thumbnail differs with a 409, so the
    // retry matches what the first request carried.
    expect(seen.sends[0]).not.toHaveProperty('thumbnail_upload_id')
    expect(seen.sends[1]).not.toHaveProperty('thumbnail_upload_id')
    expect(seen.sends[1].info).toEqual(VIDEO_INFO)
    expect(seen.sends[1].txn_id).toBe(seen.sends[0].txn_id)
  })

  it('retries bare after the server refuses the info or poster', async () => {
    let attempt = 0
    const seen = serve({ sendStatus: () => (++attempt === 1 ? 400 : 200) })
    const store = makeStore(undefined, supporting())

    expect(await store.sendMedia(mov())).toBe(false)
    expect(await store.retrySend(store.events.value[0].event_id)).toBe(true)

    expect(seen.sends[0]).toMatchObject({
      info: VIDEO_INFO,
      thumbnail_upload_id: POSTER_ID,
    })
    expect(seen.sends[1]).not.toHaveProperty('info')
    expect(seen.sends[1]).not.toHaveProperty('thumbnail_upload_id')
    expect(seen.staged.filter((s) => s.kind === 'image')).toHaveLength(1)
  })

  it('gives each send in a batch its own txn_id', async () => {
    const seen = serve()
    const store = makeStore(undefined, supporting())

    await store.sendMediaBatch([mov(), mov()])

    expect(seen.sends).toHaveLength(2)
    expect(seen.sends[0].txn_id).not.toBe(seen.sends[1].txn_id)
  })
})
