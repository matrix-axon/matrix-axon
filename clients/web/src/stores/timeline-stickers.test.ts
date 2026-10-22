import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, expect, it } from 'vitest'
import { createApiClient } from '../api/client'
import { createMediaService } from '../media/media-service'
import { createTimelineStore, type EventDto } from './timeline'

const BASE = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const STICKER = {
  body: 'Wave',
  url: 'mxc://hs/wave',
  info: { w: 320, h: 200, mimetype: 'image/gif', is_animated: true },
}
const SEND = `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/send-sticker`
const EVENTS = `${BASE}/v1/accounts/${ACCOUNT}/events/:eventId`
const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function makeStore(threadRoot?: string) {
  const auth = {
    getToken: () => 'test',
    onAuthFailure: () => {},
    LoginBootstrap: () => null,
  }
  return createTimelineStore(
    createApiClient(auth, BASE),
    createMediaService({ auth, baseUrl: BASE }),
    ACCOUNT,
    ROOM,
    threadRoot,
  )
}
function event(id = '$sticker', url = STICKER.url): EventDto {
  return {
    account_id: ACCOUNT,
    room_id: ROOM,
    event_id: id,
    sender: '@me:hs',
    type: 'm.sticker',
    body: STICKER.body,
    content: { ...STICKER, url },
    origin_ts: 100,
    arrival_order: 100,
    redacted: false,
    edited: false,
    edit_count: 0,
  } as unknown as EventDto
}

it('sends a standalone sticker with metadata and reply/thread context', async () => {
  let body: unknown
  server.use(
    http.post(SEND, async ({ request }) => {
      body = await request.json()
      return HttpResponse.json({ data: { event_id: '$sticker' } })
    }),
    http.get(EVENTS, () => HttpResponse.json({ data: event() })),
  )
  const store = makeStore('$root')
  const pending = store.sendSticker(STICKER, {
    replyTo: '$reply',
    senderId: '@me:hs',
  })
  expect(store.events.value[0]).toMatchObject({
    type: 'm.sticker',
    content: STICKER,
    localEcho: { status: 'pending', sticker: STICKER },
    relates_to: {
      rel_type: 'm.thread',
      event_id: '$root',
      'm.in_reply_to': { event_id: '$reply' },
    },
  })
  expect(await pending).toBe(true)
  expect(body).toEqual({ ...STICKER, reply_to: '$reply', thread_root: '$root' })
  expect(store.events.value.map((e) => e.event_id)).toEqual(['$sticker'])
})

it('retries failed stickers using the sticker route and preserves the selection', async () => {
  server.use(
    http.post(SEND, () =>
      HttpResponse.json(
        { error: { code: 'unavailable', message: 'Offline' } },
        { status: 503 },
      ),
    ),
  )
  const store = makeStore()
  expect(await store.sendSticker(STICKER, { senderId: '@me:hs' })).toBe(false)
  const failed = store.events.value[0]
  expect(failed.localEcho?.status).toBe('failed')
  let body: unknown
  server.use(
    http.post(SEND, async ({ request }) => {
      body = await request.json()
      return HttpResponse.json({ data: { event_id: '$sticker' } })
    }),
    http.get(EVENTS, () => HttpResponse.json({ data: event() })),
  )
  expect(await store.retrySend(failed.event_id)).toBe(true)
  expect(body).toEqual({ ...STICKER, reply_to: null, thread_root: null })
  expect(store.events.value).toHaveLength(1)
  expect(store.events.value[0].localEcho).toBeUndefined()
})

it('live confirmations distinguish text, sticker URLs, and reply context', async () => {
  // A send succeeded but its confirmed event has not reached the API yet.
  server.use(
    http.post(SEND, () =>
      HttpResponse.json({ data: { event_id: '$sticker' } }),
    ),
    http.post(`${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`, () =>
      HttpResponse.json({ data: { event_id: '$text' } }),
    ),
    http.get(EVENTS, () => HttpResponse.json({}, { status: 404 })),
  )
  const store = makeStore()
  await store.send(STICKER.body, { senderId: '@me:hs' })
  await store.sendSticker(STICKER, { senderId: '@me:hs', replyTo: '$reply' })
  store.ingestLive(event('$other', 'mxc://hs/other'))
  store.ingestLive(event('$wrong-reply'))
  expect(store.events.value.filter((e) => e.localEcho)).toHaveLength(2)
  store.ingestLive({
    ...event(),
    relates_to: { 'm.in_reply_to': { event_id: '$reply' } },
  } as unknown as EventDto)
  expect(store.events.value.filter((e) => e.localEcho)).toHaveLength(1)
  expect(store.events.value.find((e) => e.localEcho)?.type).toBe(
    'm.room.message',
  )
})
