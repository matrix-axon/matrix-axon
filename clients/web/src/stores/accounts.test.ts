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
import { createApiClient } from '../api/client'
import { createAccountsStore } from './accounts'

const BASE_URL = 'http://axon.test'

const ALICE = {
  account_id: '6b53f7f0-0000-4000-8000-000000000001',
  user_id: '@alice:example.org',
  homeserver_url: 'https://matrix.example.org',
  state: 'active',
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
}
const BOB = {
  ...ALICE,
  account_id: '6b53f7f0-0000-4000-8000-000000000002',
  user_id: '@bob:example.org',
  state: 'deactivated',
}

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function makeStore() {
  const api = createApiClient(
    {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
  return createAccountsStore(api)
}

/** Register the standard list handler; returns a counter of list fetches. */
function listAccounts(accounts: unknown[]): { calls: number } {
  const counter = { calls: 0 }
  server.use(
    http.get(`${BASE_URL}/v1/accounts`, () => {
      counter.calls += 1
      return HttpResponse.json({ data: accounts })
    }),
  )
  return counter
}

describe('refresh', () => {
  it('loads the account list and clears loading', async () => {
    listAccounts([ALICE, BOB])
    const store = makeStore()
    expect(store.loading.value).toBe(true)

    await store.refresh()

    expect(store.loading.value).toBe(false)
    expect(store.accounts.value.map((a) => a.user_id)).toEqual([
      '@alice:example.org',
      '@bob:example.org',
    ])
    expect(store.error.value).toBeNull()
  })

  it('surfaces the error envelope and still clears loading', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/accounts`, () =>
        HttpResponse.json(
          { error: { code: 'internal', message: 'database unavailable' } },
          { status: 500 },
        ),
      ),
    )
    const store = makeStore()
    await store.refresh()

    expect(store.loading.value).toBe(false)
    expect(store.error.value).toBe('database unavailable')
    expect(store.accounts.value).toEqual([])
  })
})

describe('login', () => {
  it('POSTs credentials, refreshes on success', async () => {
    let body: unknown
    const counter = listAccounts([ALICE])
    server.use(
      http.post(`${BASE_URL}/v1/accounts/login`, async ({ request }) => {
        body = await request.json()
        return HttpResponse.json({ data: ALICE })
      }),
    )

    const store = makeStore()
    const result = await store.login({
      username: '@alice:example.org',
      password: 'hunter2',
    })

    expect(result).toEqual({ ok: true })
    expect(body).toEqual({
      username: '@alice:example.org',
      password: 'hunter2',
      homeserver_url: null,
    })
    expect(counter.calls).toBe(1)
    expect(store.accounts.value).toHaveLength(1)
    expect(store.pending.value).toBeNull()
  })

  it('imports a recovery key after login and refreshes once', async () => {
    let loginBody: unknown
    let recoverBody: unknown
    const counter = listAccounts([{ ...ALICE, verified: true }])
    server.use(
      http.post(`${BASE_URL}/v1/accounts/login`, async ({ request }) => {
        loginBody = await request.json()
        return HttpResponse.json({ data: ALICE })
      }),
      http.post(
        `${BASE_URL}/v1/accounts/${ALICE.account_id}/recover`,
        async ({ request }) => {
          recoverBody = await request.json()
          return HttpResponse.json({
            data: {
              ...ALICE,
              verified: true,
              backup_action: 'enabled',
            },
          })
        },
      ),
    )

    const store = makeStore()
    const result = await store.login({
      username: '@alice:example.org',
      password: 'hunter2',
      recovery_key: ' EsTc secret ',
    })

    expect(result).toEqual({
      ok: true,
      recover: { ok: true, verified: true, backupAction: 'enabled' },
    })
    expect(loginBody).toEqual({
      username: '@alice:example.org',
      password: 'hunter2',
      homeserver_url: null,
    })
    expect(recoverBody).toEqual({ recovery_key: 'EsTc secret' })
    expect(counter.calls).toBe(1)
    expect(store.error.value).toBeNull()
    expect(store.accounts.value[0].verified).toBe(true)
  })

  it('refreshes the logged-in account and preserves recovery errors', async () => {
    const counter = listAccounts([ALICE])
    server.use(
      http.post(`${BASE_URL}/v1/accounts/login`, () =>
        HttpResponse.json({ data: ALICE }),
      ),
      http.post(`${BASE_URL}/v1/accounts/${ALICE.account_id}/recover`, () =>
        HttpResponse.json(
          { error: { code: 'bad_request', message: 'wrong recovery key' } },
          { status: 400 },
        ),
      ),
    )

    const store = makeStore()
    const result = await store.login({
      username: '@alice:example.org',
      password: 'hunter2',
      recovery_key: 'EsTc bad',
    })

    expect(result.ok).toBe(false)
    expect(counter.calls).toBe(1)
    expect(store.accounts.value).toHaveLength(1)
    expect(store.error.value).toBe('wrong recovery key')
  })

  it('surfaces upstream failure without refreshing', async () => {
    const counter = listAccounts([])
    server.use(
      http.post(`${BASE_URL}/v1/accounts/login`, () =>
        HttpResponse.json(
          { error: { code: 'bad_gateway', message: 'homeserver unreachable' } },
          { status: 502 },
        ),
      ),
    )

    const store = makeStore()
    const result = await store.login({ username: '@a:b.c', password: 'x' })

    expect(result.ok).toBe(false)
    expect(store.error.value).toBe('homeserver unreachable')
    expect(counter.calls).toBe(0)
  })
})

describe('logout / recover / remove', () => {
  it('logout hits the account-scoped route and refreshes', async () => {
    const counter = listAccounts([BOB])
    server.use(
      http.post(`${BASE_URL}/v1/accounts/${ALICE.account_id}/logout`, () =>
        HttpResponse.json({ data: BOB }),
      ),
    )

    const store = makeStore()
    expect(await store.logout(ALICE.account_id)).toBe(true)
    expect(counter.calls).toBe(1)
  })

  it('recover trims the key, refreshes, and reports the re-derived verified', async () => {
    let body: unknown
    const counter = listAccounts([{ ...ALICE, verified: true }])
    server.use(
      http.post(
        `${BASE_URL}/v1/accounts/${ALICE.account_id}/recover`,
        async ({ request }) => {
          body = await request.json()
          return HttpResponse.json({
            data: {
              ...ALICE,
              verified: true,
              backup_action: 'joined',
            },
          })
        },
      ),
    )

    const store = makeStore()
    const result = await store.recover(ALICE.account_id, '  EsTc secret \n')

    expect(result).toEqual({
      ok: true,
      verified: true,
      backupAction: 'joined',
    })
    expect(body).toEqual({ recovery_key: 'EsTc secret' })
    expect(counter.calls).toBe(1)
    expect(store.error.value).toBeNull()
  })

  it('recover reports the unverified partial-backup case', async () => {
    listAccounts([{ ...ALICE, verified: false }])
    server.use(
      http.post(`${BASE_URL}/v1/accounts/${ALICE.account_id}/recover`, () =>
        HttpResponse.json({ data: { ...ALICE, verified: false } }),
      ),
    )

    const store = makeStore()
    expect(await store.recover(ALICE.account_id, 'EsTc secret')).toEqual({
      ok: true,
      verified: false,
      backupAction: undefined,
    })
  })

  it('recover surfaces a wrong-key 400 without refreshing', async () => {
    server.use(
      http.post(`${BASE_URL}/v1/accounts/${ALICE.account_id}/recover`, () =>
        HttpResponse.json(
          { error: { code: 'bad_request', message: 'wrong recovery key' } },
          { status: 400 },
        ),
      ),
    )

    const store = makeStore()
    expect((await store.recover(ALICE.account_id, 'EsTc bad')).ok).toBe(false)
    expect(store.error.value).toBe('wrong recovery key')
  })

  it('remove handles the bodyless 204 and refreshes', async () => {
    const counter = listAccounts([])
    server.use(
      http.delete(
        `${BASE_URL}/v1/accounts/${ALICE.account_id}`,
        () => new HttpResponse(null, { status: 204 }),
      ),
    )

    const store = makeStore()
    expect(await store.remove(ALICE.account_id)).toBe(true)
    expect(counter.calls).toBe(1)
    expect(store.accounts.value).toEqual([])
  })

  it('a second action is refused while one is pending', async () => {
    listAccounts([ALICE])
    server.use(
      http.post(`${BASE_URL}/v1/accounts/login`, async () => {
        await new Promise((resolve) => setTimeout(resolve, 25))
        return HttpResponse.json({ data: ALICE })
      }),
    )

    const store = makeStore()
    const first = store.login({ username: '@a:b.c', password: 'x' })
    await vi.waitFor(() => expect(store.pending.value).not.toBeNull())

    expect(await store.login({ username: '@a:b.c', password: 'x' })).toEqual({
      ok: false,
    })
    expect(await first).toEqual({ ok: true })
    expect(store.pending.value).toBeNull()
  })
})

describe('enableBackup', () => {
  it('omits recovery_key when the buffer is empty', async () => {
    let body: unknown
    listAccounts([{ ...ALICE, verified: true }])
    server.use(
      http.post(
        `${BASE_URL}/v1/accounts/${ALICE.account_id}/backup/enable`,
        async ({ request }) => {
          body = await request.json()
          return HttpResponse.json({
            data: {
              ...ALICE,
              verified: true,
              backup_action: 'already_uploading',
            },
          })
        },
      ),
    )

    const store = makeStore()
    await store.refresh()
    const result = await store.enableBackup(ALICE.account_id, '  ')

    expect(result).toEqual({
      ok: true,
      backupAction: 'already_uploading',
    })
    expect(body).toEqual({})
  })

  it('trims and sends the recovery key for create/export', async () => {
    let body: unknown
    listAccounts([{ ...ALICE, verified: true }])
    server.use(
      http.post(
        `${BASE_URL}/v1/accounts/${ALICE.account_id}/backup/enable`,
        async ({ request }) => {
          body = await request.json()
          return HttpResponse.json({
            data: { ...ALICE, verified: true, backup_action: 'enabled' },
          })
        },
      ),
    )

    const store = makeStore()
    await store.refresh()
    const result = await store.enableBackup(
      ALICE.account_id,
      '  EsTc secret \n',
    )

    expect(result).toEqual({ ok: true, backupAction: 'enabled' })
    expect(body).toEqual({ recovery_key: 'EsTc secret' })
  })

  it('refuses an unverified account before the key is sent', async () => {
    let sent = false
    listAccounts([{ ...ALICE, verified: false }])
    server.use(
      http.post(
        `${BASE_URL}/v1/accounts/${ALICE.account_id}/backup/enable`,
        () => {
          sent = true
          return HttpResponse.json({ data: ALICE })
        },
      ),
    )

    const store = makeStore()
    await store.refresh()
    const result = await store.enableBackup(ALICE.account_id, 'EsTc secret')

    expect(result.ok).toBe(false)
    expect(sent).toBe(false)
    expect(store.error.value).toMatch(/not verified/)
  })

  it('surfaces a 409 without refreshing', async () => {
    const counter = listAccounts([{ ...ALICE, verified: true }])
    server.use(
      http.post(
        `${BASE_URL}/v1/accounts/${ALICE.account_id}/backup/enable`,
        () =>
          HttpResponse.json(
            {
              error: {
                code: 'conflict',
                message: 'recover first to join',
              },
            },
            { status: 409 },
          ),
      ),
    )

    const store = makeStore()
    await store.refresh()
    expect(counter.calls).toBe(1)
    expect((await store.enableBackup(ALICE.account_id, 'EsTc secret')).ok).toBe(
      false,
    )
    expect(store.error.value).toBe('recover first to join')
    expect(counter.calls).toBe(1)
  })
})
