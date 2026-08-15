import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createApiClient } from '../api/client'
import { memoryStorage } from '../test/memory-storage'
import { createInvitesStore, type InviteDto } from './invites'
import { createRoomsStore } from './rooms'

const BASE = 'http://axon.test'
const ACCOUNT = '11111111-1111-1111-1111-111111111111'

function invite(roomId: string, name: string): InviteDto {
  return {
    account_id: ACCOUNT,
    account_user_id: '@me:hs',
    room_id: roomId,
    name,
    avatar_url: null,
    topic: null,
    canonical_alias: null,
    room_type: null,
    inviter_user_id: '@alice:hs',
    inviter_display_name: 'Alice',
    is_direct: false,
    encrypted: true,
    invited_at: '2026-08-14T12:00:00+00:00',
  }
}

describe('invites store', () => {
  const server = setupServer()
  beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
  afterEach(() => server.resetHandlers())
  afterAll(() => server.close())

  function store() {
    const api = createApiClient(
      {
        getToken: () => 'tok-test',
        onAuthFailure: () => {},
        LoginBootstrap: () => null,
      },
      BASE,
    )
    const rooms = createRoomsStore(api, memoryStorage())
    return createInvitesStore(api, rooms)
  }

  it('loads invites and reports the count', async () => {
    server.use(
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] }),
      ),
    )
    const invites = store()
    await invites.refresh()
    expect(invites.count.value).toBe(1)
    expect(invites.invites.value[0].name).toBe('Alpha')
    expect(invites.loading.value).toBe(false)
  })

  it('drops a row on accept and calls join', async () => {
    const joined: string[] = []
    server.use(
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        }),
      ),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          const body = (await request.json()) as { room_id_or_alias: string }
          joined.push(body.room_id_or_alias)
          return HttpResponse.json({ data: { room_id: body.room_id_or_alias } })
        },
      ),
      http.get(`${BASE}/v1/rooms`, () => HttpResponse.json({ data: [] })),
    )
    const invites = store()
    await invites.refresh()
    const result = await invites.accept(invites.invites.value[0])
    expect(result).toEqual({ ok: true })
    expect(joined).toEqual(['!a:hs'])
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!b:hs'])
  })

  it('drops a row on reject and ignores a stale GET that still lists it', async () => {
    const left: string[] = []
    server.use(
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        }),
      ),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        ({ params }) => {
          left.push(String(params.roomId))
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const invites = store()
    await invites.refresh()
    const result = await invites.reject(invites.invites.value[0])
    expect(result).toEqual({ ok: true })
    expect(left).toEqual(['!a:hs'])
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!b:hs'])
  })

  it('keeps the row when reject fails', async () => {
    server.use(
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] }),
      ),
      http.post(`${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`, () =>
        HttpResponse.json(
          { error: { code: 'not_found', message: 'room not found' } },
          { status: 404 },
        ),
      ),
    )
    const invites = store()
    await invites.refresh()
    const result = await invites.reject(invites.invites.value[0])
    expect(result.ok).toBe(false)
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])
  })

  it('rejectAll leaves every invite even when a later GET is stale', async () => {
    const left: string[] = []
    let inviteReads = 0
    server.use(
      http.get(`${BASE}/v1/invites`, () => {
        inviteReads += 1
        // After both leaves, a lagged watcher still returns the second row.
        if (left.length >= 2) {
          return HttpResponse.json({ data: [invite('!b:hs', 'Beta')] })
        }
        return HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        })
      }),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        ({ params }) => {
          left.push(String(params.roomId))
          return HttpResponse.json({ data: {} })
        },
      ),
      http.get(`${BASE}/v1/rooms`, () => HttpResponse.json({ data: [] })),
    )
    const invites = store()
    await invites.refresh()
    const result = await invites.rejectAll()
    expect(result).toEqual({ succeeded: 2, failed: [] })
    expect(left).toEqual(['!a:hs', '!b:hs'])
    expect(inviteReads).toBeGreaterThan(1)
    expect(invites.invites.value).toEqual([])
  })

  it('rejects without aborting the rest of rejectAll', async () => {
    server.use(
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        }),
      ),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        ({ params }) => {
          if (params.roomId === '!a:hs') {
            return HttpResponse.json(
              { error: { code: 'forbidden', message: 'nope' } },
              { status: 403 },
            )
          }
          return HttpResponse.json({ data: {} })
        },
      ),
      http.get(`${BASE}/v1/rooms`, () => HttpResponse.json({ data: [] })),
    )
    const invites = store()
    await invites.refresh()
    const result = await invites.rejectAll()
    expect(result.succeeded).toBe(1)
    expect(result.failed).toHaveLength(1)
    expect(result.failed[0].invite.room_id).toBe('!a:hs')
  })

  it('applies live added and removed frames', () => {
    const invites = store()
    invites.noteAdded(invite('!a:hs', 'Alpha'))
    expect(invites.count.value).toBe(1)
    invites.noteRemoved(ACCOUNT, '!a:hs')
    expect(invites.count.value).toBe(0)
  })

  it('a server-driven removal leaves no suppression behind', async () => {
    // `invite.removed` says the row is already gone upstream, so there is
    // nothing for a later GET to wrongly restore and nothing to suppress.
    // Recording it as locally-resolved is a trap: every later GET that does
    // carry the key keeps the suppression alive, so the row can never come
    // back — the ADR's "GET /v1/invites is the reconnect source of truth"
    // stops holding.
    server.use(
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] }),
      ),
    )
    const invites = store()
    invites.noteRemoved(ACCOUNT, '!a:hs')
    expect(invites.count.value).toBe(0)
    await invites.refresh()
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])
    await invites.refresh()
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])
  })

  it('shows a re-invite after a local reject', async () => {
    // Same shape, but the suppression is legitimate at first: we rejected
    // locally, so a lagging GET must not restore the row. An `invite.added`
    // is the server saying it exists *now*, which outranks that.
    server.use(
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        () => new HttpResponse(null, { status: 200 }),
      ),
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] }),
      ),
    )
    const invites = store()
    await invites.refresh()
    await invites.reject(invite('!a:hs', 'Alpha'))
    expect(invites.count.value).toBe(0)

    invites.noteAdded(invite('!a:hs', 'Alpha'))
    expect(invites.count.value).toBe(1)
    await invites.refresh()
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])
  })

  it('keeps an invite added while a GET was in flight', async () => {
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(`${BASE}/v1/invites`, async () => {
        await held
        return HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] })
      }),
    )
    const invites = store()
    const pending = invites.refresh()
    // The response was already on the wire, so it cannot carry this one.
    invites.noteAdded(invite('!b:hs', 'Beta'))
    release?.()
    await pending
    expect(invites.invites.value.map((row) => row.room_id).sort()).toEqual([
      '!a:hs',
      '!b:hs',
    ])
  })

  it('does not treat a single live frame as a settled list', async () => {
    // The initial GET fails, then one frame arrives. `rejectAll` must still
    // confirm the list rather than act on the single row it can see.
    let attempt = 0
    server.use(
      http.get(`${BASE}/v1/invites`, () => {
        attempt += 1
        return attempt === 1
          ? new HttpResponse(null, { status: 503 })
          : HttpResponse.json({
              data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
            })
      }),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        () => new HttpResponse(null, { status: 200 }),
      ),
    )
    const invites = store()
    await invites.refresh()
    expect(invites.error.value).not.toBeNull()

    invites.noteAdded(invite('!a:hs', 'Alpha'))
    expect(invites.count.value).toBe(1)

    const result = await invites.rejectAll()
    expect(result.succeeded).toBe(2)
  })

  it('refresh after a mutation does not resolve against an older GET', async () => {
    const gets: (() => void)[] = []
    let served = 0
    server.use(
      http.get(`${BASE}/v1/invites`, async () => {
        served += 1
        const mine = served
        await new Promise<void>((resolve) => gets.push(resolve))
        // Only the second GET knows the invite is gone.
        return HttpResponse.json({
          data: mine === 1 ? [invite('!a:hs', 'Alpha')] : [],
        })
      }),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        () => new HttpResponse(null, { status: 200 }),
      ),
    )
    /** Wait until the handler for GET number `n` is parked. */
    async function waitForGet(n: number): Promise<void> {
      for (let i = 0; i < 200 && gets.length < n; i += 1) {
        await new Promise((resolve) => setTimeout(resolve, 5))
      }
      expect(gets.length).toBeGreaterThanOrEqual(n)
    }

    const invites = store()
    const first = invites.refresh()
    await waitForGet(1)
    // A second caller arrives while the first GET is still in flight; it must
    // get a fetch issued after this point, not the one already on the wire.
    const second = invites.refresh()
    expect(second).not.toBe(first)

    gets[0]()
    await first
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])

    await waitForGet(2)
    gets[1]()
    await second
    expect(served).toBe(2)
    expect(invites.invites.value).toEqual([])
  })

  it('discards a refresh that finishes after resetSession', async () => {
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(`${BASE}/v1/invites`, async () => {
        await held
        return HttpResponse.json({ data: [invite('!a:hs', 'Stale')] })
      }),
    )
    const invites = store()
    const pending = invites.refresh()
    invites.resetSession()
    release?.()
    await pending
    expect(invites.invites.value).toEqual([])
  })
})
