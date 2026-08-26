import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { effect } from '@preact/signals'
import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  it,
  vi,
} from 'vitest'
import { createApiClient } from '../api/client'
import { memoryStorage } from '../test/memory-storage'
import { isLikelyDm, roomKey, roomTitle } from './room-list'
import { createRoomsStore } from './rooms'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'

const NAMED = {
  account_id: ACCOUNT,
  account_user_id: '@me:example.org',
  room_id: '!ops:hs',
  name: 'Ops',
  last_activity_ts: 100,
  notification_count: 0,
  highlight_count: 0,
}
const UNNAMED = {
  account_id: ACCOUNT,
  account_user_id: '@me:example.org',
  room_id: '!dm:hs',
  last_activity_ts: 200,
  notification_count: 0,
  highlight_count: 0,
}

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function makeStore(storage = memoryStorage()) {
  const api = createApiClient(
    {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
  return createRoomsStore(api, storage)
}

/** Mirrors `ROOM_TITLE_CACHE_KEY`, which the store keeps private. */
const TITLE_CACHE_KEY = 'axon.room_titles.v1'

/** One named room plus a DM whose title resolves to `Bob` from its members. */
function useTitleHandlers(): void {
  server.use(
    http.get(`${BASE_URL}/v1/rooms`, () =>
      HttpResponse.json({ data: [NAMED, UNNAMED] }),
    ),
    http.get(
      `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent('!dm:hs')}/members`,
      () =>
        HttpResponse.json({
          data: [
            {
              user_id: '@me:example.org',
              membership: 'join',
              display_name: 'Me',
            },
            {
              user_id: '@bob:example.org',
              membership: 'join',
              display_name: 'Bob',
            },
          ],
        }),
    ),
  )
}

describe('createRoomsStore', () => {
  it('loads rooms and resolves member-derived titles for unnamed rooms', async () => {
    let memberCalls = 0
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [NAMED, UNNAMED] }),
      ),
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent('!dm:hs')}/members`,
        () => {
          memberCalls += 1
          return HttpResponse.json({
            data: [
              {
                user_id: '@me:example.org',
                membership: 'join',
                display_name: 'Me',
              },
              {
                user_id: '@bob:example.org',
                membership: 'join',
                display_name: 'Bob',
              },
            ],
          })
        },
      ),
    )

    const store = makeStore()
    await store.refresh()

    expect(store.loading.value).toBe(false)
    expect(store.rooms.value).toHaveLength(2)
    await vi.waitFor(() =>
      expect(store.titles.value.get(roomKey(UNNAMED))).toBe('Bob'),
    )
    expect(store.dmAvatars.value.get(roomKey(UNNAMED))).toBeUndefined()
    // Named rooms never trigger a members fetch.
    expect(memberCalls).toBe(1)

    // A second refresh does not re-fetch settled titles.
    await store.refresh()
    await new Promise((resolve) => setTimeout(resolve, 10))
    expect(memberCalls).toBe(1)
  })

  it('caches the other user’s avatar for a 1:1 DM with no room avatar', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [NAMED, UNNAMED] }),
      ),
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent('!dm:hs')}/members`,
        () =>
          HttpResponse.json({
            data: [
              {
                user_id: '@me:example.org',
                membership: 'join',
              },
              {
                user_id: '@bob:example.org',
                membership: 'join',
                display_name: 'Bob',
                avatar_url: 'mxc://hs/bob',
              },
            ],
          }),
      ),
    )

    const store = makeStore()
    await store.refresh()
    await vi.waitFor(() =>
      expect(store.dmAvatars.value.get(roomKey(UNNAMED))).toBe('mxc://hs/bob'),
    )
    expect(store.dmAvatars.value.has(roomKey(NAMED))).toBe(false)

    store.resetSession()
    expect(store.dmAvatars.value.size).toBe(0)
  })

  it('persists room titles and starts the next store from the cache', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [NAMED, UNNAMED] }),
      ),
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent('!dm:hs')}/members`,
        () =>
          HttpResponse.json({
            data: [
              {
                user_id: '@me:example.org',
                membership: 'join',
                display_name: 'Me',
              },
              {
                user_id: '@bob:example.org',
                membership: 'join',
                display_name: 'Bob',
              },
            ],
          }),
      ),
    )
    const storage = memoryStorage()
    const first = makeStore(storage)
    await first.refresh()
    await vi.waitFor(() =>
      expect(first.titles.value.get(roomKey(UNNAMED))).toBe('Bob'),
    )

    const second = makeStore(storage)
    expect(second.titles.value.get(roomKey(NAMED))).toBe('Ops')
    expect(second.titles.value.get(roomKey(UNNAMED))).toBe('Bob')
  })

  it('drops the persisted title cache when the session ends', async () => {
    useTitleHandlers()
    const storage = memoryStorage()
    const store = makeStore(storage)
    await store.refresh()
    await vi.waitFor(() =>
      expect(store.titles.value.get(roomKey(UNNAMED))).toBe('Bob'),
    )
    expect(storage.getItem(TITLE_CACHE_KEY)).not.toBeNull()

    store.resetSession()

    // For a DM this is the correspondent's display name. Leaving it readable in
    // localStorage lets the next user of a shared browser see who the previous
    // one was talking to, with no session left to expire it (#115).
    expect(storage.getItem(TITLE_CACHE_KEY)).toBeNull()
    expect(store.titles.value.size).toBe(0)

    // The cache is only an optimization: a new session re-resolves from
    // `/members` without it (WCR-02).
    const next = makeStore(storage)
    expect(next.titles.value.size).toBe(0)
    await next.refresh()
    await vi.waitFor(() =>
      expect(next.titles.value.get(roomKey(UNNAMED))).toBe('Bob'),
    )
  })

  it('does not re-persist a title from a members fetch that lands after sign-out', async () => {
    let release: (() => void) | undefined
    const inFlight = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [UNNAMED] }),
      ),
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent('!dm:hs')}/members`,
        async () => {
          await inFlight
          return HttpResponse.json({
            data: [
              {
                user_id: '@me:example.org',
                membership: 'join',
                display_name: 'Me',
              },
              {
                user_id: '@bob:example.org',
                membership: 'join',
                display_name: 'Bob',
              },
            ],
          })
        },
      ),
    )
    const storage = memoryStorage()
    const store = makeStore(storage)
    await store.refresh()

    // Sign out with the `/members` burst still in flight.
    store.resetSession()
    expect(storage.getItem(TITLE_CACHE_KEY)).toBeNull()

    release?.()
    await new Promise((resolve) => setTimeout(resolve, 20))

    // `setCachedTitle` persists, so an unguarded completion here re-creates the
    // record sign-out just removed — with the previous reader's DM name in it.
    expect(storage.getItem(TITLE_CACHE_KEY)).toBeNull()
    expect(store.titles.value.size).toBe(0)
  })

  it('drops the persisted title cache even before the first refresh lands', () => {
    // The pristine-store early return in `resetSession` skips the rest of the
    // wipe, but `titles` was already loaded from storage at construction — so
    // signing out mid-first-refresh must still clear it.
    const storage = memoryStorage({
      [TITLE_CACHE_KEY]: JSON.stringify({
        version: 1,
        titles: [[roomKey(UNNAMED), 'Bob']],
      }),
    })
    const store = makeStore(storage)
    expect(store.titles.value.get(roomKey(UNNAMED))).toBe('Bob')

    store.resetSession()

    expect(storage.getItem(TITLE_CACHE_KEY)).toBeNull()
    expect(store.titles.value.size).toBe(0)
  })

  it('keeps the room-id fallback when the members fetch fails', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [UNNAMED] }),
      ),
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent('!dm:hs')}/members`,
        () =>
          HttpResponse.json(
            { error: { code: 'not_found', message: 'no such room' } },
            { status: 404 },
          ),
      ),
    )

    const store = makeStore()
    await store.refresh()
    await new Promise((resolve) => setTimeout(resolve, 10))

    expect(store.titles.value.has(roomKey(UNNAMED))).toBe(false)
    expect(store.error.value).toBeNull()
  })

  it('noteActivity advances last_activity_ts and ignores older stamps (WCR-08)', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [NAMED, UNNAMED] }),
      ),
      http.get(`${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/members`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const store = makeStore()
    await store.refresh()

    // NAMED (ts 100) is not the freshest; a newer stamp re-publishes the list.
    const before = store.rooms.value
    store.noteActivity(ACCOUNT, NAMED.room_id, 500)
    expect(store.rooms.value).not.toBe(before)
    expect(
      store.rooms.value.find((r) => r.room_id === NAMED.room_id)!
        .last_activity_ts,
    ).toBe(500)

    // An older or equal stamp is inert.
    const after = store.rooms.value
    store.noteActivity(ACCOUNT, NAMED.room_id, 400)
    store.noteActivity(ACCOUNT, NAMED.room_id, 500)
    expect(store.rooms.value).toBe(after)
  })

  it('a burst in the already-freshest room does not re-publish the list (WCR-08)', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [NAMED, UNNAMED] }),
      ),
      http.get(`${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/members`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const store = makeStore()
    await store.refresh()

    // Make NAMED the freshest, then burst it: the first bump re-sorts, the
    // rest must not trigger an O(rooms) copy + full list re-render each.
    store.noteActivity(ACCOUNT, NAMED.room_id, 500)
    const published = store.rooms.value
    store.noteActivity(ACCOUNT, NAMED.room_id, 600)
    store.noteActivity(ACCOUNT, NAMED.room_id, 700)
    expect(store.rooms.value).toBe(published)
    // The stamp still advanced (in place) so ordering stays correct.
    expect(
      store.rooms.value.find((r) => r.room_id === NAMED.room_id)!
        .last_activity_ts,
    ).toBe(700)
  })

  it('updates a room preview from a live message without a timeline refetch', async () => {
    let timelineCalls = 0
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [NAMED] }),
      ),
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(NAMED.room_id)}/members`,
        () =>
          HttpResponse.json({
            data: [
              {
                user_id: '@me:example.org',
                membership: 'join',
                display_name: 'Me',
              },
              {
                user_id: '@adam:example.org',
                membership: 'join',
                display_name: 'Adam',
              },
            ],
          }),
      ),
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(NAMED.room_id)}/timeline`,
        () => {
          timelineCalls += 1
          return HttpResponse.json({ data: { events: [], next_cursor: null } })
        },
      ),
    )
    const store = makeStore()
    await store.refresh()

    store.noteTimelineEvent({
      account_id: ACCOUNT,
      room_id: NAMED.room_id,
      event_id: '$live',
      origin_ts: 500,
      // Required by the generated contract; irrelevant to this store's
      // behavior, which keys on origin_ts (ADR 0089).
      arrival_order: 1,
      sender: '@adam:example.org',
      type: 'm.room.message',
      state_key: null,
      body: '**Hello** from [live](https://example.org)',
      content: { msgtype: 'm.text', body: 'Hello from live' } as never,
      redacted: false,
      edited: false,
      edit_count: 0,
    })

    expect(store.preview(roomKey(NAMED))?.body).toBe('Hello from live')
    expect(store.preview(roomKey(NAMED))?.senderDisplay).toBe(
      '@adam:example.org',
    )
    await vi.waitFor(() =>
      expect(store.preview(roomKey(NAMED))?.senderDisplay).toBe('Adam'),
    )
    expect(timelineCalls).toBe(0)
  })

  it('a live event tying last_activity_ts still updates the preview', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [NAMED] }),
      ),
      http.get(`${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/members`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const store = makeStore()
    await store.refresh()

    // Same millisecond as the refreshed last_activity_ts (a send burst, or a
    // frame racing a refresh that already recorded its ts): the newer event
    // must still become the preview.
    store.noteTimelineEvent({
      account_id: ACCOUNT,
      room_id: NAMED.room_id,
      event_id: '$tie',
      origin_ts: NAMED.last_activity_ts,
      // Required by the generated contract; irrelevant to this store's
      // behavior, which keys on origin_ts (ADR 0089).
      arrival_order: 2,
      sender: '@adam:example.org',
      type: 'm.room.message',
      state_key: null,
      body: 'Tied timestamp',
      content: { msgtype: 'm.text', body: 'Tied timestamp' } as never,
      redacted: false,
      edited: false,
      edit_count: 0,
    })

    expect(store.preview(roomKey(NAMED))?.body).toBe('Tied timestamp')
  })

  it('a live preview update wakes only that room preview subscriber', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [NAMED, UNNAMED] }),
      ),
      http.get(`${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/members`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const store = makeStore()
    await store.refresh()
    const namedKey = roomKey(NAMED)
    const unnamedKey = roomKey(UNNAMED)
    let namedRuns = 0
    let unnamedRuns = 0
    const stopNamed = effect(() => {
      store.preview(namedKey)
      namedRuns += 1
    })
    const stopUnnamed = effect(() => {
      store.preview(unnamedKey)
      unnamedRuns += 1
    })
    const namedBefore = namedRuns
    const unnamedBefore = unnamedRuns

    store.noteTimelineEvent({
      account_id: ACCOUNT,
      room_id: NAMED.room_id,
      event_id: '$live',
      origin_ts: 500,
      // Required by the generated contract; irrelevant to this store's
      // behavior, which keys on origin_ts (ADR 0089).
      arrival_order: 3,
      sender: '@adam:example.org',
      type: 'm.room.message',
      state_key: null,
      body: 'Only Ops changes',
      content: { msgtype: 'm.text', body: 'Only Ops changes' } as never,
      redacted: false,
      edited: false,
      edit_count: 0,
    })

    expect(namedRuns).toBe(namedBefore + 1)
    expect(unnamedRuns).toBe(unnamedBefore)
    stopNamed()
    stopUnnamed()
  })

  it('a non-content live event does not bump last_activity_ts or the preview', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [NAMED] }),
      ),
      http.get(`${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/members`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const store = makeStore()
    await store.refresh()

    // A redaction newer than the room's recorded activity must not become
    // the activity marker or the preview (mirrors the server-side filter in
    // `list_rooms`, #119).
    store.noteTimelineEvent({
      account_id: ACCOUNT,
      room_id: NAMED.room_id,
      event_id: '$redaction',
      origin_ts: NAMED.last_activity_ts + 1000,
      // Required by the generated contract; irrelevant to this store's
      // behavior, which keys on origin_ts (ADR 0089).
      arrival_order: 4,
      sender: '@adam:example.org',
      type: 'm.room.redaction',
      state_key: null,
      body: null,
      content: {} as never,
      redacted: false,
      edited: false,
      edit_count: 0,
    })

    const room = store.rooms.value.find((r) => r.room_id === NAMED.room_id)
    expect(room?.last_activity_ts).toBe(NAMED.last_activity_ts)
    expect(store.preview(roomKey(NAMED))).toBeUndefined()

    // Same for a membership change (join/leave/profile update).
    store.noteTimelineEvent({
      account_id: ACCOUNT,
      room_id: NAMED.room_id,
      event_id: '$member',
      origin_ts: NAMED.last_activity_ts + 2000,
      // Required by the generated contract; irrelevant to this store's
      // behavior, which keys on origin_ts (ADR 0089).
      arrival_order: 5,
      sender: '@adam:example.org',
      type: 'm.room.member',
      state_key: '@adam:example.org',
      body: null,
      content: { membership: 'join' } as never,
      redacted: false,
      edited: false,
      edit_count: 0,
    })

    const roomAfter = store.rooms.value.find((r) => r.room_id === NAMED.room_id)
    expect(roomAfter?.last_activity_ts).toBe(NAMED.last_activity_ts)
  })

  it('a non-content event for an unknown room still triggers discovery', async () => {
    let listCalls = 0
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () => {
        listCalls += 1
        return HttpResponse.json({ data: [NAMED] })
      }),
    )
    const store = makeStore()
    await store.refresh()
    expect(listCalls).toBe(1)

    // A bare join into a freshly joined, message-less room is our first
    // signal it exists — it must still trigger a re-read of the list even
    // though it doesn't count as "activity" once the room is known.
    store.noteTimelineEvent({
      account_id: ACCOUNT,
      room_id: '!new:hs',
      event_id: '$join',
      origin_ts: 900,
      // Required by the generated contract; irrelevant to this store's
      // behavior, which keys on origin_ts (ADR 0089).
      arrival_order: 6,
      sender: '@me:example.org',
      type: 'm.room.member',
      state_key: '@me:example.org',
      body: null,
      content: { membership: 'join' } as never,
      redacted: false,
      edited: false,
      edit_count: 0,
    })

    await vi.waitFor(() => expect(listCalls).toBe(2))
  })

  it('an unknown room triggers one coalesced refresh (WCR-08)', async () => {
    let listCalls = 0
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () => {
        listCalls += 1
        return HttpResponse.json({ data: [NAMED] })
      }),
    )
    const store = makeStore()
    await store.refresh()
    expect(listCalls).toBe(1)

    // A frame burst for a freshly joined room re-reads the list once.
    store.noteActivity(ACCOUNT, '!new:hs', 900)
    store.noteActivity(ACCOUNT, '!new:hs', 901)
    store.noteActivity(ACCOUNT, '!new:hs', 902)
    await vi.waitFor(() => expect(listCalls).toBe(2))
    expect(listCalls).toBe(2)
  })

  it('leaves a room through M19b, hides it immediately, and refreshes authoritative rooms', async () => {
    let leaveCalls = 0
    let listCalls = 0
    let listedRooms = [NAMED]
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () => {
        listCalls += 1
        return HttpResponse.json({ data: listedRooms })
      }),
      http.post(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(NAMED.room_id)}/leave`,
        () => {
          leaveCalls += 1
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const store = makeStore()
    await store.refresh()
    expect(store.rooms.value).toEqual([NAMED])

    const result = await store.leaveRoom(ACCOUNT, NAMED.room_id)

    expect(result).toEqual({ ok: true })
    expect(leaveCalls).toBe(1)
    expect(listCalls).toBe(2)
    // The immediate refresh can race sync and still return the room. Keep it
    // hidden locally once the leave mutation itself has succeeded.
    expect(store.rooms.value).toEqual([])

    // If the same room id appears on a later refresh without an intervening
    // absence, treat that as a legitimate rejoin and show it again.
    await store.refresh()
    expect(store.rooms.value).toEqual([NAMED])

    listedRooms = []
    await store.refresh()
    expect(store.rooms.value).toEqual([])

    listedRooms = [NAMED]
    await store.refresh()
    expect(store.rooms.value).toEqual([NAMED])
  })

  it('returns a recoverable message when forgetting fails', async () => {
    server.use(
      http.post(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(NAMED.room_id)}/forget`,
        () =>
          HttpResponse.json(
            {
              error: {
                code: 'bad_request',
                message: "room isn't left or banned",
              },
            },
            { status: 400 },
          ),
      ),
    )
    const store = makeStore()

    const result = await store.forgetRoom(ACCOUNT, NAMED.room_id)

    expect(result).toEqual({
      ok: false,
      message: "room isn't left or banned",
    })
    expect(store.error.value).toBe("room isn't left or banned")
  })

  it('joins a room through M19c and refreshes authoritative rooms', async () => {
    let joinBody: unknown = null
    let listCalls = 0
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () => {
        listCalls += 1
        return HttpResponse.json({ data: [NAMED] })
      }),
      http.post(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: NAMED.room_id } })
        },
      ),
    )
    const store = makeStore()

    const result = await store.joinRoom(ACCOUNT, '#ops:hs', ['hs', 'backup'])

    expect(result).toEqual({ ok: true, roomId: NAMED.room_id })
    expect(joinBody).toEqual({
      room_id_or_alias: '#ops:hs',
      server_names: ['hs', 'backup'],
    })
    expect(listCalls).toBe(1)
    expect(store.rooms.value).toEqual([NAMED])
  })

  it('knocks on a room through M19c with an optional reason', async () => {
    let knockBody: unknown = null
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () => HttpResponse.json({ data: [] })),
      http.post(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/knock`,
        async ({ request }) => {
          knockBody = await request.json()
          return HttpResponse.json({ data: { room_id: '!private:hs' } })
        },
      ),
    )
    const store = makeStore()

    const result = await store.knockRoom(ACCOUNT, '#private:hs', 'let me in', [
      'hs',
    ])

    expect(result).toEqual({ ok: true, roomId: '!private:hs' })
    expect(knockBody).toEqual({
      room_id_or_alias: '#private:hs',
      reason: 'let me in',
      server_names: ['hs'],
    })
  })

  it('creates a room through M19c and refreshes authoritative rooms', async () => {
    let createBody: unknown = null
    let listCalls = 0
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () => {
        listCalls += 1
        return HttpResponse.json({ data: [NAMED] })
      }),
      http.post(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms`,
        async ({ request }) => {
          createBody = await request.json()
          return HttpResponse.json({ data: { room_id: NAMED.room_id } })
        },
      ),
    )
    const store = makeStore()

    const result = await store.createRoom(ACCOUNT, {
      name: 'Ops',
      topic: 'Runbooks',
      invite: ['@bob:hs'],
      is_direct: false,
      public: false,
      preset: 'private_chat',
      encrypted: true,
    })

    expect(result).toEqual({ ok: true, roomId: NAMED.room_id })
    expect(createBody).toEqual({
      name: 'Ops',
      topic: 'Runbooks',
      invite: ['@bob:hs'],
      is_direct: false,
      public: false,
      preset: 'private_chat',
      encrypted: true,
    })
    expect(listCalls).toBe(1)
    expect(store.rooms.value).toHaveLength(1)
    expect(store.rooms.value[0]).toMatchObject({
      ...NAMED,
      topic: 'Runbooks',
    })
  })

  it('keeps create-room metadata while sync catches up to the room name', async () => {
    const createdRoom = {
      account_id: ACCOUNT,
      account_user_id: '@me:example.org',
      room_id: '!created:hs',
      name: null,
      topic: null,
      canonical_alias: null,
      last_activity_ts: 300,
      notification_count: 0,
      highlight_count: 0,
    }
    let memberCalls = 0
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [createdRoom] }),
      ),
      http.get(
        `${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/members`,
        () => {
          memberCalls += 1
          return HttpResponse.json({
            data: [
              { user_id: '@me:example.org', membership: 'join' },
              {
                user_id: '@bob:example.org',
                membership: 'join',
                display_name: 'Bob',
              },
            ],
          })
        },
      ),
      http.post(`${BASE_URL}/v1/accounts/${ACCOUNT}/rooms`, () =>
        HttpResponse.json({ data: { room_id: createdRoom.room_id } }),
      ),
    )
    const store = makeStore()

    const result = await store.createRoom(ACCOUNT, {
      name: 'Project',
      topic: 'Launch planning',
      invite: ['@bob:example.org'],
      is_direct: false,
      public: false,
      preset: 'private_chat',
      encrypted: true,
    })

    expect(result).toEqual({ ok: true, roomId: createdRoom.room_id })
    expect(store.rooms.value[0]).toMatchObject({
      room_id: createdRoom.room_id,
      name: 'Project',
      topic: 'Launch planning',
    })
    expect(isLikelyDm(store.rooms.value[0])).toBe(false)
    expect(roomTitle(store.rooms.value[0], store.titles.value)).toBe('Project')
    await new Promise((resolve) => setTimeout(resolve, 10))
    expect(memberCalls).toBe(0)
  })

  it('creates a DM through M19c and surfaces errors without retrying', async () => {
    let dmCalls = 0
    let dmBody: unknown = null
    server.use(
      http.post(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`,
        async ({ request }) => {
          dmCalls += 1
          dmBody = await request.json()
          return HttpResponse.json(
            { error: { code: 'forbidden', message: 'blocked' } },
            { status: 403 },
          )
        },
      ),
    )
    const store = makeStore()

    const result = await store.createDm(ACCOUNT, '@bob:hs')

    expect(result).toEqual({
      ok: false,
      message: 'blocked',
      code: 'forbidden',
    })
    expect(dmBody).toEqual({ user_id: '@bob:hs' })
    expect(dmCalls).toBe(1)
    expect(store.error.value).toBe('blocked')
  })

  it('opens an existing one-to-one DM instead of creating a duplicate', async () => {
    let dmCalls = 0
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [UNNAMED] }),
      ),
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(UNNAMED.room_id)}/members`,
        () =>
          HttpResponse.json({
            data: [
              {
                user_id: '@me:example.org',
                membership: 'join',
                display_name: 'Me',
              },
              {
                user_id: '@bob:example.org',
                membership: 'join',
                display_name: 'Bob',
              },
            ],
          }),
      ),
      http.post(`${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`, () => {
        dmCalls += 1
        return HttpResponse.json({ data: { room_id: '!new-dm:hs' } })
      }),
    )
    const store = makeStore()

    await store.refresh()
    const result = await store.createDm(ACCOUNT, '@bob:example.org')

    expect(result).toEqual({ ok: true, roomId: UNNAMED.room_id })
    expect(dmCalls).toBe(0)
  })

  it('surfaces a list-fetch error', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json(
          { error: { code: 'internal', message: 'database unavailable' } },
          { status: 500 },
        ),
      ),
    )
    const store = makeStore()
    await store.refresh()

    expect(store.error.value).toBe('database unavailable')
    expect(store.loading.value).toBe(false)
  })

  it('hydrates unread counts from room summaries', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [{ ...NAMED, notification_count: 4, highlight_count: 1 }],
        }),
      ),
      http.get(`${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/members`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const store = makeStore()
    await store.refresh()

    const key = roomKey(NAMED)
    expect(store.unreadCount(key)).toBe(4)
    expect([...store.unreadKeys.value]).toEqual([key])
    expect(store.unreadTotal.value).toBe(4)
  })

  it('applies live unread count updates per room', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [NAMED, UNNAMED] }),
      ),
      http.get(`${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/members`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const store = makeStore()
    await store.refresh()
    const key = roomKey(NAMED)

    store.noteUnreadCounts(ACCOUNT, NAMED.room_id, 2, 0)
    expect(store.unreadCount(key)).toBe(2)
    expect(store.unreadKeys.value.has(key)).toBe(true)

    store.noteUnreadCounts(ACCOUNT, NAMED.room_id, 0, 0)
    expect(store.unreadCount(key)).toBe(0)
    expect(store.unreadKeys.value.has(key)).toBe(false)
  })

  it('unreadTotal sums notification counts across rooms and keeps growing within one room (ADR 0080)', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [NAMED, UNNAMED] }),
      ),
      http.get(`${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/members`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const store = makeStore()
    await store.refresh()
    expect(store.unreadTotal.value).toBe(0)

    // Unlike `unreadKeys.size`, further messages in an already-unread room
    // keep raising the total instead of sticking at 1.
    store.noteUnreadCounts(ACCOUNT, NAMED.room_id, 1, 0)
    expect(store.unreadTotal.value).toBe(1)
    store.noteUnreadCounts(ACCOUNT, NAMED.room_id, 3, 0)
    expect(store.unreadTotal.value).toBe(3)

    // A second room's count adds on top of the first's.
    store.noteUnreadCounts(ACCOUNT, UNNAMED.room_id, 2, 0)
    expect(store.unreadTotal.value).toBe(5)

    // Reading one room back down subtracts only its own share.
    store.noteUnreadCounts(ACCOUNT, NAMED.room_id, 0, 0)
    expect(store.unreadTotal.value).toBe(2)
  })
})
