import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createApiClient } from '../api/client'
import { setPerfEnabled, setTelemetrySink } from '../perf'
import { createMemberDirectory } from './member-directory'
import { createMembersStore, createMembersStoreCache } from './members'
import type { MemberDto } from './room-list'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:example.org'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function makeApi() {
  return createApiClient(
    {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
}

const membersPath = (roomId: string) =>
  `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(roomId)}/members`

function named(user_id: string, display_name: string): MemberDto {
  return {
    user_id,
    membership: 'join',
    display_name,
    avatar_url: `mxc://hs/${display_name}`,
  } as MemberDto
}

describe('member names across rooms', () => {
  it("borrows a name from another room until this room's own list lands", async () => {
    const OTHER = '!other:example.org'
    let release: () => void = () => {}
    let arrived: () => void = () => {}
    const requested = new Promise<void>((resolve) => {
      arrived = resolve
    })
    server.use(
      http.get(membersPath(OTHER), () =>
        HttpResponse.json({ data: [named('@alice:example.org', 'Alice')] }),
      ),
      http.get(
        membersPath(ROOM),
        () =>
          new Promise<Response>((resolve) => {
            arrived()
            release = () =>
              resolve(
                HttpResponse.json({
                  data: [named('@alice:example.org', 'Alice (here)')],
                }) as Response,
              )
          }),
      ),
    )
    const directory = createMemberDirectory()
    const api = makeApi()
    await createMembersStore(api, ACCOUNT, OTHER, directory).refresh()

    const store = createMembersStore(api, ACCOUNT, ROOM, directory)
    const refreshing = store.refresh()
    expect(store.displayName('@alice:example.org')).toBe('Alice')
    expect(store.profile('@alice:example.org')?.avatar_url).toBe(
      'mxc://hs/Alice',
    )
    // The roster itself does not claim Alice before the room says so.
    expect(store.members.value.has('@alice:example.org')).toBe(false)
    // Nobody has a name for a stranger: the raw id, as before.
    expect(store.displayName('@zed:example.org')).toBe('@zed:example.org')

    await requested
    release()
    await refreshing
    // Per-room names: this room's own entry wins once it lands.
    expect(store.displayName('@alice:example.org')).toBe('Alice (here)')
  })

  it("lends no other room's name once this room's list has landed", async () => {
    const OTHER = '!other:example.org'
    server.use(
      http.get(membersPath(OTHER), () =>
        HttpResponse.json({ data: [named('@alice:example.org', 'Alice')] }),
      ),
      http.get(membersPath(ROOM), () =>
        HttpResponse.json({ data: [named('@bob:example.org', 'Bob')] }),
      ),
    )
    const directory = createMemberDirectory()
    const api = makeApi()
    await createMembersStore(api, ACCOUNT, OTHER, directory).refresh()

    const store = createMembersStore(api, ACCOUNT, ROOM, directory)
    await store.refresh()

    // Alice is not in this room's list (a bridge ghost, a sender who never
    // joined): the room's answer is her id, not the name another room gave.
    expect(store.displayName('@alice:example.org')).toBe('@alice:example.org')
    expect(store.profile('@alice:example.org')).toBeUndefined()
    expect(store.displayName('@bob:example.org')).toBe('Bob')
  })

  it('keeps lending names while the room has no list of its own', async () => {
    const OTHER = '!other:example.org'
    server.use(
      http.get(membersPath(OTHER), () =>
        HttpResponse.json({ data: [named('@alice:example.org', 'Alice')] }),
      ),
      http.get(membersPath(ROOM), () =>
        HttpResponse.json(
          { error: { code: 'upstream', message: 'down' } },
          { status: 502 },
        ),
      ),
    )
    const directory = createMemberDirectory()
    const api = makeApi()
    await createMembersStore(api, ACCOUNT, OTHER, directory).refresh()

    const store = createMembersStore(api, ACCOUNT, ROOM, directory)
    await store.refresh()

    // A failed read is no answer, so the fallback still stands.
    expect(store.error.value).not.toBeNull()
    expect(store.displayName('@alice:example.org')).toBe('Alice')
  })

  it('hands a room its warm store back, names and all', async () => {
    server.use(
      http.get(membersPath(ROOM), () =>
        HttpResponse.json({ data: [named('@alice:example.org', 'Alice')] }),
      ),
    )
    const cache = createMembersStoreCache(makeApi(), createMemberDirectory(), 2)
    const first = cache.acquire(ACCOUNT, ROOM)
    await first.refresh()
    cache.acquire(ACCOUNT, '!b:example.org')

    const again = cache.acquire(ACCOUNT, ROOM)
    expect(again === first).toBe(true)
    expect(again.loading.value).toBe(false)
    expect(again.displayName('@alice:example.org')).toBe('Alice')

    // Past the cap, the least recently acquired room goes.
    cache.acquire(ACCOUNT, '!c:example.org')
    cache.acquire(ACCOUNT, '!d:example.org')
    expect(cache.size).toBe(2)
    expect(cache.acquire(ACCOUNT, ROOM) === first).toBe(false)
  })
})

function makeStore() {
  const api = createApiClient(
    {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
  return createMembersStore(api, ACCOUNT, ROOM)
}

describe('createMembersStore', () => {
  it('refreshes the roster after a partial invite failure', async () => {
    const invited: string[] = []
    let memberReads = 0
    server.use(
      http.post(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/invite`,
        async ({ request }) => {
          const body = (await request.json()) as { user_id: string }
          if (body.user_id === '@bob:example.org') {
            return HttpResponse.json(
              {
                error: {
                  code: 'bad_request',
                  message: 'invalid user id',
                },
              },
              { status: 400 },
            )
          }
          invited.push(body.user_id)
          return HttpResponse.json({ data: {} })
        },
      ),
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/members`,
        () => {
          memberReads += 1
          return HttpResponse.json({
            data: [
              {
                user_id: '@alice:example.org',
                membership: 'invite',
                display_name: 'Alice',
                avatar_url: null,
              },
            ],
          })
        },
      ),
    )
    const store = makeStore()

    const result = await store.inviteUsers([
      '@alice:example.org',
      '@bob:example.org',
    ])

    expect(result).toEqual({
      ok: false,
      message: 'invalid user id',
      code: 'bad_request',
    })
    expect(invited).toEqual(['@alice:example.org'])
    expect(memberReads).toBe(1)
    expect(store.members.value.get('@alice:example.org')?.membership).toBe(
      'invite',
    )
    expect(store.error.value).toBe('invalid user id')
  })

  /**
   * A `/members` handler that holds each request until the test releases it,
   * answering with whatever list it is released with.
   */
  function heldMembers(roomId: string): {
    arrived: () => Promise<void>
    release: (index: number, list: MemberDto[]) => void
  } {
    const held: ((list: MemberDto[]) => void)[] = []
    let waiters: (() => void)[] = []
    server.use(
      http.get(
        membersPath(roomId),
        () =>
          new Promise<Response>((resolve) => {
            held.push((list) => resolve(HttpResponse.json({ data: list })))
            for (const wake of waiters.splice(0)) {
              wake()
            }
          }),
      ),
    )
    return {
      arrived: () =>
        new Promise<void>((resolve) => {
          waiters = [...waiters, resolve]
        }),
      release: (index, list) => held[index](list),
    }
  }

  it('lets no older list overwrite a newer one', async () => {
    const members = heldMembers(ROOM)
    const directory = createMemberDirectory()
    const store = createMembersStore(makeApi(), ACCOUNT, ROOM, directory)

    let arrived = members.arrived()
    const older = store.refresh()
    await arrived
    arrived = members.arrived()
    const newer = store.refresh()
    await arrived

    members.release(1, [named('@alice:example.org', 'Alice B.')])
    await newer
    // The previous visit's list, answering last on a slow link.
    members.release(0, [named('@alice:example.org', 'Alice')])
    await older

    expect(store.displayName('@alice:example.org')).toBe('Alice B.')
    expect(directory.lookup(ACCOUNT, '@alice:example.org')?.display_name).toBe(
      'Alice B.',
    )
  })

  it('ends the read once, for the newest of two overlapping refreshes', async () => {
    const members = heldMembers(ROOM)
    const store = createMembersStore(makeApi(), ACCOUNT, ROOM)
    const marks: { name: string; detail: Record<string, unknown> }[] = []
    setPerfEnabled(true)
    setTelemetrySink((name, _at, detail) => {
      if (name.startsWith('members:refresh:')) {
        marks.push({ name, detail: detail ?? {} })
      }
    })
    try {
      let arrived = members.arrived()
      const older = store.refresh()
      await arrived
      arrived = members.arrived()
      const newer = store.refresh()
      await arrived

      // The overtaken request answers last, as the previous visit's would.
      members.release(1, [named('@alice:example.org', 'Alice B.')])
      await newer
      members.release(0, [
        named('@alice:example.org', 'Alice'),
        named('@bob:example.org', 'Bob'),
      ])
      await older
    } finally {
      setTelemetrySink(null)
      setPerfEnabled(false)
    }

    // Every start is answered, by the newest list: the room-open summary
    // takes the first end, so an older one would stamp it with stale timing.
    expect(marks.map((mark) => mark.name)).toEqual([
      'members:refresh:start',
      'members:refresh:start',
      'members:refresh:end',
    ])
    expect(marks[2].detail).toMatchObject({ members: 1, ok: true })
  })

  it('puts no names back from a list that lands after sign-out', async () => {
    const members = heldMembers(ROOM)
    const directory = createMemberDirectory()
    const cache = createMembersStoreCache(makeApi(), directory)
    const arrived = members.arrived()
    const refreshing = cache.acquire(ACCOUNT, ROOM).refresh()
    await arrived

    // What `connectRoomCachesReset` does on sign-out.
    cache.clear()
    directory.clear()
    members.release(0, [named('@alice:example.org', 'Alice')])
    await refreshing

    expect(directory.size).toBe(0)
  })
})
