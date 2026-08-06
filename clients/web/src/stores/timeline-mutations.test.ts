import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createApiClient } from '../api/client'
import { createMediaService } from '../media/media-service'
import { createTimelineStore, type EventDto } from './timeline'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const TIMELINE_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/timeline`
const EVENTS_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`
const ROOM_EVENT_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/events/:eventId`

function event(id: string, ts: number, overrides: object = {}): EventDto {
  return {
    account_id: ACCOUNT,
    event_id: id,
    room_id: ROOM,
    sender: '@alice:hs',
    origin_ts: ts,
    arrival_order: ts,
    type: 'm.room.message',
    body: `body of ${id}`,
    redacted: false,
    edited: false,
    edit_count: 0,
    ...overrides,
  } as EventDto
}

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function makeStore(threadRoot?: string) {
  const auth = {
    getToken: () => 'tok-test',
    onAuthFailure: () => {},
    LoginBootstrap: () => null,
  }
  const api = createApiClient(auth, BASE_URL)
  const media = createMediaService({ auth, baseUrl: BASE_URL })
  return createTimelineStore(api, media, ACCOUNT, ROOM, threadRoot)
}

describe('send', () => {
  it('posts plain prose, echoes it locally, and reconciles with the confirmed event', async () => {
    let sendBody: unknown
    server.use(
      http.post(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        async ({ request }) => {
          sendBody = await request.json()
          return HttpResponse.json({ data: { event_id: '$new' } })
        },
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({ data: event('$new', 100) }),
      ),
    )

    const store = makeStore()
    const result = store.send('hello world')

    // The local echo renders synchronously, before the POST resolves.
    expect(store.events.value).toHaveLength(1)
    expect(store.events.value[0].event_id.startsWith('local:')).toBe(true)
    expect(store.events.value[0].localEcho?.status).toBe('pending')

    expect(await result).toBe(true)

    expect(sendBody).toEqual({
      body: 'hello world',
      reply_to: null,
      thread_root: null,
    })
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$new'])
    expect(store.events.value[0].localEcho).toBeUndefined()
  })

  it('adds markdown-derived formatted_body and the reply relation', async () => {
    let sendBody: Record<string, unknown> = {}
    server.use(
      http.post(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        async ({ request }) => {
          sendBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$new' } })
        },
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({ data: event('$new', 100) }),
      ),
    )

    const store = makeStore()
    await store.send('**bold** move', { replyTo: '$target' })

    expect(sendBody.body).toBe('**bold** move')
    expect(sendBody.format).toBe('org.matrix.custom.html')
    expect(sendBody.formatted_body).toBe('<p><strong>bold</strong> move</p>')
    expect(sendBody.reply_to).toBe('$target')
  })

  it('uses caller-provided formatted_body for sends and local echoes', async () => {
    let sendBody: Record<string, unknown> = {}
    server.use(
      http.post(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        async ({ request }) => {
          sendBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$new' } })
        },
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({ data: event('$new', 100) }),
      ),
    )

    const store = makeStore()
    const sending = store.send('hello @alice', {
      formattedBody:
        'hello <a class="mention-pill" href="https://matrix.to/#/%40alice%3Ahs">@alice</a>',
    })

    expect(
      (store.events.value[0].content as { formatted_body?: string })
        .formatted_body,
    ).toContain('mention-pill')
    await sending

    expect(sendBody.body).toBe('hello @alice')
    expect(sendBody.format).toBe('org.matrix.custom.html')
    expect(sendBody.formatted_body).toContain('mention-pill')
  })

  it('a thread-scoped store reads the thread timeline and sends into it', async () => {
    let sendBody: Record<string, unknown> = {}
    let threadTimelineCalls = 0
    server.use(
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        ({ params }) => {
          threadTimelineCalls += 1
          expect(params.rootId).toBe('$root')
          return HttpResponse.json({
            data: { events: [event('$member', 100)], next_cursor: null },
          })
        },
      ),
      http.post(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        async ({ request }) => {
          sendBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$new' } })
        },
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({ data: event('$new', 200) }),
      ),
    )

    const store = makeStore('$root')
    await store.loadLatest()
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$member'])

    await store.send('into the thread')
    expect(sendBody.thread_root).toBe('$root')
    expect(threadTimelineCalls).toBe(1) // initial page load only, no reload
    expect(store.events.value.map((e) => e.event_id)).toEqual([
      '$member',
      '$new',
    ])
  })

  it('surfaces a send failure and marks the local echo failed', async () => {
    server.use(
      http.post(`${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`, () =>
        HttpResponse.json(
          { error: { code: 'server_not_ready', message: 'still syncing' } },
          { status: 503 },
        ),
      ),
    )

    const store = makeStore()
    expect(await store.send('x')).toBe(false)
    expect(store.error.value).toBe('still syncing')
    expect(store.events.value).toHaveLength(1)
    expect(store.events.value[0].localEcho?.status).toBe('failed')
  })

  it('retrySend re-sends a failed message and reconciles on success', async () => {
    let attempts = 0
    server.use(
      http.post(`${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`, () => {
        attempts += 1
        return attempts === 1
          ? HttpResponse.json(
              { error: { code: 'server_not_ready', message: 'nope' } },
              { status: 503 },
            )
          : HttpResponse.json({ data: { event_id: '$retried' } })
      }),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({ data: event('$retried', 100) }),
      ),
    )

    const store = makeStore()
    expect(await store.send('retry me')).toBe(false)
    const localId = store.events.value[0].event_id

    expect(await store.retrySend(localId)).toBe(true)
    expect(attempts).toBe(2)
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$retried'])
  })

  it('discardSend removes a failed local echo without contacting the server', async () => {
    server.use(
      http.post(`${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`, () =>
        HttpResponse.json(
          { error: { code: 'server_not_ready', message: 'nope' } },
          { status: 503 },
        ),
      ),
    )

    const store = makeStore()
    expect(await store.send('doomed')).toBe(false)
    const localId = store.events.value[0].event_id

    store.discardSend(localId)
    expect(store.events.value).toEqual([])
  })
})

describe('edit / redact', () => {
  it('edit PUTs the new body and patches the event in place', async () => {
    let editBody: Record<string, unknown> = {}
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: {
            events: [event('$b', 200), event('$a', 100)],
            next_cursor: null,
          },
        }),
      ),
      http.put(ROOM_EVENT_PATH, async ({ request }) => {
        editBody = (await request.json()) as Record<string, unknown>
        return HttpResponse.json({ data: { event_id: '$edit' } })
      }),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$a', 100, {
            body: 'fixed',
            edited: true,
            edit_count: 1,
          }),
        }),
      ),
    )

    const store = makeStore()
    await store.loadLatest()
    expect(await store.edit('$a', 'fixed')).toBe(true)

    expect(editBody.body).toBe('fixed')
    const edited = store.events.value.find((e) => e.event_id === '$a')!
    expect(edited.body).toBe('fixed')
    expect(edited.edited).toBe(true)
    // The other event is untouched and order is preserved.
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$a', '$b'])
  })

  it('redact DELETEs and shows the masked row', async () => {
    let deleted: string | null = null
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: { events: [event('$a', 100)], next_cursor: null },
        }),
      ),
      http.delete(ROOM_EVENT_PATH, ({ params }) => {
        deleted = params.eventId as string
        return new HttpResponse(null, { status: 204 })
      }),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$a', 100, {
            redacted: true,
            body: null,
            content: null,
            redaction_event_id: '$r',
          }),
        }),
      ),
    )

    const store = makeStore()
    await store.loadLatest()
    expect(await store.redact('$a')).toBe(true)

    expect(deleted).toBe('$a')
    expect(store.events.value[0].redacted).toBe(true)
  })

  it('edit still succeeds when the confirming refetch network-fails (WCR-02)', async () => {
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: { events: [event('$a', 100)], next_cursor: null },
        }),
      ),
      http.put(ROOM_EVENT_PATH, () =>
        HttpResponse.json({ data: { event_id: '$edit' } }),
      ),
      // The connection drops right after the mutation lands: the refetch is
      // best-effort, so the edit's own promise must still resolve true.
      http.get(EVENTS_PATH, () => HttpResponse.error()),
    )

    const store = makeStore()
    await store.loadLatest()
    expect(await store.edit('$a', 'fixed')).toBe(true)
    // The stale row stays until a live frame or page read heals it.
    expect(store.events.value[0].body).toBe('body of $a')
    expect(store.error.value).toBeNull()
  })

  it('uses caller-provided formatted_body for edits', async () => {
    let editBody: Record<string, unknown> = {}
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: { events: [event('$a', 100)], next_cursor: null },
        }),
      ),
      http.put(ROOM_EVENT_PATH, async ({ request }) => {
        editBody = (await request.json()) as Record<string, unknown>
        return HttpResponse.json({ data: { event_id: '$edit' } })
      }),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$a', 100, {
            body: 'fixed @alice',
            edited: true,
            edit_count: 1,
          }),
        }),
      ),
    )

    const store = makeStore()
    await store.loadLatest()
    await store.edit('$a', 'fixed @alice', {
      formattedBody:
        'fixed <a class="mention-pill" href="https://matrix.to/#/%40alice%3Ahs">@alice</a>',
    })

    expect(editBody.body).toBe('fixed @alice')
    expect(editBody.formatted_body).toContain('mention-pill')
  })

  it('send still succeeds when the reconciling refetch network-fails, leaving the echo pending (WCR-02)', async () => {
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({ data: { events: [], next_cursor: null } }),
      ),
      http.post(`${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`, () =>
        HttpResponse.json({ data: { event_id: '$new' } }),
      ),
      http.get(EVENTS_PATH, () => HttpResponse.error()),
    )

    const store = makeStore()
    await store.loadLatest()
    expect(await store.send('hello', { senderId: '@me:hs' })).toBe(true)
    const echo = store.events.value[0]
    expect(echo.localEcho?.status).toBe('pending')
    expect(echo.body).toBe('hello')
  })
})

describe('toggleReaction', () => {
  it('reacts when I have not reacted', async () => {
    let reactBody: unknown
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: { events: [event('$a', 100)], next_cursor: null },
        }),
      ),
      http.post(`${ROOM_EVENT_PATH}/reactions`, async ({ request }) => {
        reactBody = await request.json()
        return HttpResponse.json({ data: { event_id: '$rx' } })
      }),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$a', 100, {
            reactions: {
              '👍': {
                count: 1,
                me: true,
                senders: ['@me:hs'],
                my_event_ids: ['$rx'],
              },
            },
          }),
        }),
      ),
    )

    const store = makeStore()
    await store.loadLatest()
    expect(await store.toggleReaction(store.events.value[0], '👍')).toBe(true)

    expect(reactBody).toEqual({ key: '👍' })
    expect(store.events.value[0].reactions?.['👍']?.me).toBe(true)
  })

  it('redacts my reaction event when toggling off', async () => {
    let deleted: string | null = null
    const reacted = event('$a', 100, {
      reactions: {
        '👍': {
          count: 2,
          me: true,
          senders: ['@me:hs', '@bob:hs'],
          my_event_ids: ['$mine'],
        },
      },
    })
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: { events: [reacted], next_cursor: null },
        }),
      ),
      http.delete(ROOM_EVENT_PATH, ({ params }) => {
        deleted = params.eventId as string
        return new HttpResponse(null, { status: 204 })
      }),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$a', 100, {
            reactions: { '👍': { count: 1, me: false, senders: ['@bob:hs'] } },
          }),
        }),
      ),
    )

    const store = makeStore()
    await store.loadLatest()
    expect(await store.toggleReaction(store.events.value[0], '👍')).toBe(true)

    expect(deleted).toBe('$mine')
    expect(store.events.value[0].reactions?.['👍']?.me).toBe(false)
  })
})
