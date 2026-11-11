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
} from 'vitest'
import { createApiClient } from '../api/client'
import { createMediaService } from '../media/media-service'
import type { OutgoingLinkPreview } from '../link-preview-send'
import { createTimelineStore, type EventDto } from './timeline'

/**
 * A composer send that carries previews (ADR 0132 § 6–7): thumbnails are
 * uploaded afresh on each attempt, and a rejection for the previews makes
 * Retry send the text with the "no previews" marker.
 */

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const UPLOAD_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/media/uploads`
const SEND_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`
const EVENTS_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`
const A = 'https://a.example.org/x'
const B = 'https://b.example.org/y'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

/** Files handed to `media.upload`, read back. jsdom sends a `File` body as
 *  the text "undefined", so the bytes are checked here, not on the wire. */
let staged: Promise<{ type: string; bytes: number[] }>[] = []
beforeEach(() => {
  staged = []
})

function makeStore() {
  const auth = {
    getToken: () => 'tok-test',
    onAuthFailure: () => {},
    LoginBootstrap: () => null,
  }
  const media = createMediaService({ auth, baseUrl: BASE_URL })
  const upload = media.upload
  media.upload = (accountId, file) => {
    staged.push(
      file.arrayBuffer().then((buffer) => ({
        type: file.type,
        bytes: [...new Uint8Array(buffer)],
      })),
    )
    return upload(accountId, file)
  }
  return createTimelineStore(
    createApiClient(auth, BASE_URL),
    media,
    ACCOUNT,
    ROOM,
  )
}

const stagedFiles = () => Promise.all(staged)

const preview = (
  matchedUrl: string,
  image: Uint8Array<ArrayBuffer> | null,
): OutgoingLinkPreview => ({
  matchedUrl,
  title: `About ${matchedUrl}`,
  description: null,
  siteName: null,
  image: image === null ? null : { bytes: image, mimeType: 'image/jpeg' },
})

function confirmed(body: string): EventDto {
  return {
    account_id: ACCOUNT,
    event_id: '$sent',
    room_id: ROOM,
    sender: '@me:hs',
    origin_ts: 100,
    arrival_order: 100,
    type: 'm.room.message',
    body,
    content: { msgtype: 'm.text', body },
    redacted: false,
    edited: false,
    edit_count: 0,
  } as unknown as EventDto
}

/**
 * Serve uploads with fresh ids, and answer each send from `answers` in turn.
 * Records the bytes and type of every upload, and every send body.
 */
function serve(answers: (() => Response)[]) {
  const uploads: { kind: string }[] = []
  const sends: Record<string, unknown>[] = []
  let next = 0
  server.use(
    http.post(UPLOAD_PATH, async ({ request }) => {
      uploads.push({
        kind: new URL(request.url).searchParams.get('kind') ?? '',
      })
      const id = `00000000-0000-4000-8000-00000000000${uploads.length}`
      return HttpResponse.json({ data: { upload_id: id } })
    }),
    http.post(SEND_PATH, async ({ request }) => {
      sends.push((await request.json()) as Record<string, unknown>)
      return answers[next++]()
    }),
    http.get(EVENTS_PATH, () =>
      HttpResponse.json({ data: confirmed(`see ${A}`) }),
    ),
  )
  return { uploads, sends }
}

const ok = () => HttpResponse.json({ data: { event_id: '$sent' } })
const rejected = (status: number, code: string) => () =>
  HttpResponse.json({ error: { code, message: 'no' } }, { status })

describe('preview sends', () => {
  it('uploads each thumbnail and sends its new id with the text', async () => {
    const { uploads, sends } = serve([ok])
    const store = makeStore()
    expect(
      await store.send(`see ${A} and ${B}`, {
        formattedBody: null,
        linkPreviews: [preview(A, new Uint8Array([1, 2, 3])), preview(B, null)],
      }),
    ).toBe(true)
    expect(uploads).toEqual([{ kind: 'image' }])
    expect(await stagedFiles()).toEqual([
      { type: 'image/jpeg', bytes: [1, 2, 3] },
    ])
    expect(sends[0].url_previews).toEqual([
      {
        matched_url: A,
        title: `About ${A}`,
        image_upload_id: '00000000-0000-4000-8000-000000000001',
      },
      { matched_url: B, title: `About ${B}` },
    ])
  })

  it('sends a card without its image when the thumbnail upload fails', async () => {
    const { sends } = serve([ok])
    server.use(
      http.post(UPLOAD_PATH, () =>
        HttpResponse.json(
          { error: { code: 'bad_request', message: 'no' } },
          { status: 400 },
        ),
      ),
    )
    const store = makeStore()
    await store.send(`see ${A}`, {
      formattedBody: null,
      linkPreviews: [preview(A, new Uint8Array([1]))],
    })
    expect(sends[0].url_previews).toEqual([
      { matched_url: A, title: `About ${A}` },
    ])
  })

  it('sends the "no previews" marker as []', async () => {
    const { sends, uploads } = serve([ok])
    await makeStore().send(`see ${A}`, {
      formattedBody: null,
      linkPreviews: [],
    })
    expect(sends[0].url_previews).toEqual([])
    expect(uploads).toEqual([])
  })

  it('leaves the field out without previews', async () => {
    const { sends } = serve([ok])
    await makeStore().send(`see ${A}`, { formattedBody: null })
    expect(sends[0]).not.toHaveProperty('url_previews')
  })

  it.each([
    [413, 'preview_too_large'],
    [413, 'event_too_large'],
    [413, 'payload_too_large'],
    [400, 'bad_request'],
    [404, 'not_found'],
  ])(
    'records a %s %s as a preview rejection, and Retry sends []',
    async (status, code) => {
      const { sends, uploads } = serve([rejected(status, code), ok])
      const store = makeStore()
      expect(
        await store.send(`see ${A}`, {
          formattedBody: null,
          linkPreviews: [preview(A, new Uint8Array([1]))],
        }),
      ).toBe(false)
      const [failed] = store.events.value
      expect(failed.localEcho).toMatchObject({
        status: 'failed',
        previewsRejected: true,
      })
      expect(await store.retrySend(failed.event_id)).toBe(true)
      expect(sends.map((send) => send.url_previews)).toEqual([
        [
          {
            matched_url: A,
            title: `About ${A}`,
            image_upload_id: '00000000-0000-4000-8000-000000000001',
          },
        ],
        [],
      ])
      expect(uploads).toHaveLength(1)
    },
  )

  it('keeps the cards through any other failure, and Retry uploads their thumbnails again', async () => {
    const { sends, uploads } = serve([rejected(502, 'upstream_error'), ok])
    const store = makeStore()
    await store.send(`see ${A}`, {
      formattedBody: null,
      linkPreviews: [preview(A, new Uint8Array([7]))],
    })
    const [failed] = store.events.value
    expect(failed.localEcho?.status).toBe('failed')
    expect(failed.localEcho?.previewsRejected).toBeUndefined()
    expect(await store.retrySend(failed.event_id)).toBe(true)
    // Two uploads, two different ids: the first attempt's upload is never
    // sent again.
    expect(uploads).toHaveLength(2)
    expect((await stagedFiles()).map((file) => file.bytes)).toEqual([[7], [7]])
    expect(
      sends.map(
        (send) =>
          (send.url_previews as { image_upload_id?: string }[])[0]
            .image_upload_id,
      ),
    ).toEqual([
      '00000000-0000-4000-8000-000000000001',
      '00000000-0000-4000-8000-000000000002',
    ])
  })

  it('does not call a rejection of a send without previews a preview rejection', async () => {
    serve([rejected(400, 'bad_request')])
    const store = makeStore()
    await store.send(`see ${A}`, { formattedBody: null })
    expect(store.events.value[0].localEcho?.previewsRejected).toBeUndefined()
  })
})
