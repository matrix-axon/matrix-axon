import { computed, signal, type Signal } from '@preact/signals'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { ApiClient } from '../api/client'
import { memoryStorage } from '../test/memory-storage'
import type { Account, AccountsStore } from './accounts'
import {
  createMatrixOAuthBrowserStore,
  type MatrixOAuthBrowserFlow,
  type MatrixOAuthBrowserStore,
} from './matrix-oauth-browser'

const FLOW_ID = '10000000-0000-4000-8000-000000000001'
const FLOW_ID_2 = '20000000-0000-4000-8000-000000000002'
const STORAGE_KEY = 'axon.matrix-oauth-browser.flow-id'
const LINK = 'https://auth.example.org/link?code=ABCD-EFGH'
const CODE = 'ABCD-EFGH'

function flow(
  stage: MatrixOAuthBrowserFlow['stage'] = 'starting',
  overrides: Partial<MatrixOAuthBrowserFlow> = {},
): MatrixOAuthBrowserFlow {
  return {
    flow_id: FLOW_ID,
    expected_user_id: '@alice:example.org',
    stage,
    ...overrides,
  }
}

function waiting(): MatrixOAuthBrowserFlow {
  return flow('waiting_for_authorization', {
    verification_uri: LINK,
    user_code: CODE,
  })
}

function response(status = 200): Response {
  return new Response(null, { status })
}

function success(data: MatrixOAuthBrowserFlow, status = 200) {
  return Promise.resolve({ data: { data }, response: response(status) })
}

function refusal(status: number, code: string, message: string) {
  return Promise.resolve({
    error: { error: { code, message } },
    response: response(status),
  })
}

function apiMock() {
  return {
    GET: vi.fn(),
    POST: vi.fn(),
    DELETE: vi.fn(),
  }
}

function accountsMock(listed: Signal<Account[]>): AccountsStore {
  return {
    accounts: computed(() => listed.value),
    loading: computed(() => false),
    error: signal(null),
    pending: computed(() => null),
    refresh: vi.fn().mockResolvedValue(undefined),
    login: vi.fn(),
    logout: vi.fn(),
    recover: vi.fn(),
    enableBackup: vi.fn(),
    remove: vi.fn(),
  }
}

const stores: Array<Pick<MatrixOAuthBrowserStore, 'reset'>> = []
afterEach(() => {
  for (const store of stores) {
    store.reset()
  }
  stores.length = 0
  vi.useRealTimers()
})

function create(
  api: ReturnType<typeof apiMock>,
  options: {
    storage?: Storage
    requestTimeoutMs?: number
    pollDelayMs?: number
    transportBackoffMs?: number
  } = {},
) {
  const listed = signal<Account[]>([])
  const accounts = accountsMock(listed)
  const store = createMatrixOAuthBrowserStore(
    api as unknown as ApiClient,
    accounts,
    options,
  )
  stores.push(store)
  return { accounts, listed, store }
}

describe('MatrixOAuthBrowserStore', () => {
  it('starts with only the user ID, stores only the flow id, and polls', async () => {
    vi.useFakeTimers()
    const storage = memoryStorage()
    const api = apiMock()
    api.POST.mockReturnValue(success(flow('starting'), 201))
    api.GET.mockReturnValue(success(waiting()))
    const { store } = create(api, { storage, pollDelayMs: 1 })

    await expect(store.start('@alice:example.org')).resolves.toBe(true)
    expect(api.POST).toHaveBeenCalledWith('/v1/accounts/login/browser', {
      body: { expected_user_id: '@alice:example.org' },
      signal: expect.any(AbortSignal),
    })
    expect(store.hasStoredFlow()).toBe(true)

    await vi.advanceTimersByTimeAsync(1)
    expect(api.GET).toHaveBeenCalledWith(
      '/v1/accounts/login/browser/{flow_id}',
      {
        params: { path: { flow_id: FLOW_ID } },
        signal: expect.any(AbortSignal),
      },
    )
    expect(store.flow.value?.verification_uri).toBe(LINK)
    expect(store.flow.value?.user_code).toBe(CODE)
    // The link and the code are bearer-ish: neither may outlive the tab's
    // memory.
    expect(storage.length).toBe(1)
    expect(storage.getItem(STORAGE_KEY)).toBe(FLOW_ID)
  })

  it('refreshes accounts once on done, forgets the flow id, and stops polling', async () => {
    vi.useFakeTimers()
    const storage = memoryStorage()
    const api = apiMock()
    api.POST.mockReturnValue(success(waiting(), 201))
    api.GET.mockReturnValue(success(flow('done', { account_id: 'account-1' })))
    const { accounts, store } = create(api, { storage, pollDelayMs: 1 })

    await store.start('@alice:example.org')
    await vi.advanceTimersByTimeAsync(1)
    expect(store.flow.value?.stage).toBe('done')
    expect(store.flow.value?.verification_uri).toBeUndefined()
    expect(accounts.refresh).toHaveBeenCalledTimes(1)
    expect(store.hasStoredFlow()).toBe(false)
    expect(store.operation.value).toBe('idle')

    await vi.advanceTimersByTimeAsync(60_000)
    expect(api.GET).toHaveBeenCalledTimes(1)
  })

  it.each([
    ['unsupported', /does not support browser sign-in/],
    ['authorization_denied', /declined/],
    ['authorization_expired', /expired/],
    ['user_mismatch', /different account/],
    ['a_code_added_later', /Browser sign-in failed/],
    [null, /Browser sign-in failed/],
  ])('explains a failed flow with code %s', async (code, message) => {
    const api = apiMock()
    api.POST.mockReturnValue(success(flow('failed', { error_code: code }), 201))
    const { accounts, store } = create(api)

    await store.start('@alice:example.org')
    expect(store.error.value).toMatch(message)
    expect(store.hasStoredFlow()).toBe(false)
    expect(accounts.refresh).not.toHaveBeenCalled()
  })

  it("shows the server's own reason when it refuses to create a flow", async () => {
    const api = apiMock()
    api.POST.mockReturnValue(
      refusal(409, 'conflict', 'Another sign-in is open for this user'),
    )
    const { store } = create(api)

    await expect(store.start('@alice:example.org')).resolves.toBe(false)
    expect(store.flow.value).toBeNull()
    expect(store.error.value).toBe('Another sign-in is open for this user')
    expect(store.operation.value).toBe('idle')
  })

  it('resumes a stored flow after a reload', async () => {
    const storage = memoryStorage({ [STORAGE_KEY]: FLOW_ID })
    const api = apiMock()
    api.GET.mockReturnValue(success(waiting()))
    const { store } = create(api, { storage, pollDelayMs: 60_000 })

    expect(store.hasStoredFlow()).toBe(true)
    await expect(store.resume()).resolves.toBe(true)
    expect(store.flow.value?.user_code).toBe(CODE)
  })

  it('drops a stored flow the server no longer knows', async () => {
    const storage = memoryStorage({ [STORAGE_KEY]: FLOW_ID })
    const api = apiMock()
    api.GET.mockReturnValue(refusal(404, 'not_found', 'unknown flow'))
    const { accounts, store } = create(api, { storage })

    await expect(store.resume()).resolves.toBe(false)
    expect(store.flow.value).toBeNull()
    expect(store.hasStoredFlow()).toBe(false)
    // Whose sign-in it was is unknown after a reload, so the page cannot say
    // whether it finished; it can only point at the list it just refreshed.
    expect(store.error.value).toMatch(/If its account is listed above/)
    expect(accounts.refresh).toHaveBeenCalledTimes(1)
  })

  it('recognises a sign-in that finished while the page was away', async () => {
    vi.useFakeTimers()
    const api = apiMock()
    api.POST.mockReturnValue(success(waiting(), 201))
    // Axon forgets a finished flow after two minutes.
    api.GET.mockReturnValue(refusal(404, 'not_found', 'unknown flow'))
    const { accounts, listed, store } = create(api, { pollDelayMs: 1 })
    vi.mocked(accounts.refresh).mockImplementation(() => {
      listed.value = [
        {
          account_id: 'account-1',
          user_id: '@alice:example.org',
          state: 'active',
        } as Account,
      ]
      return Promise.resolve()
    })

    await store.start('@alice:example.org')
    await vi.advanceTimersByTimeAsync(1)

    expect(store.flow.value).toEqual({
      flow_id: FLOW_ID,
      expected_user_id: '@alice:example.org',
      stage: 'done',
      account_id: 'account-1',
    })
    expect(store.error.value).toBeNull()
    expect(store.hasStoredFlow()).toBe(false)
    expect(store.operation.value).toBe('idle')
  })

  it('reports a flow that vanished without signing the account in', async () => {
    vi.useFakeTimers()
    const api = apiMock()
    api.POST.mockReturnValue(success(waiting(), 201))
    api.GET.mockReturnValue(refusal(404, 'not_found', 'unknown flow'))
    const { accounts, listed, store } = create(api, { pollDelayMs: 1 })
    // Another account being active proves nothing about this one.
    listed.value = [
      {
        account_id: 'account-2',
        user_id: '@bob:example.org',
        state: 'active',
      } as Account,
      {
        account_id: 'account-1',
        user_id: '@alice:example.org',
        state: 'deactivated',
      } as Account,
    ]

    await store.start('@alice:example.org')
    await vi.advanceTimersByTimeAsync(1)

    expect(store.flow.value).toBeNull()
    expect(store.error.value).toBe(
      'This browser sign-in is no longer open. Start again to continue.',
    )
    expect(accounts.refresh).toHaveBeenCalledTimes(1)
  })

  it('keeps waiting through a failed poll and retries after the backoff', async () => {
    vi.useFakeTimers()
    const api = apiMock()
    api.POST.mockReturnValue(success(waiting(), 201))
    api.GET.mockRejectedValueOnce(new TypeError('network down'))
    api.GET.mockReturnValue(success(flow('finalizing')))
    const { store } = create(api, { pollDelayMs: 1, transportBackoffMs: 50 })

    await store.start('@alice:example.org')
    await vi.advanceTimersByTimeAsync(1)
    expect(store.error.value).toMatch(/Could not reach Axon/)
    // The link stays usable: the user may be mid-approval.
    expect(store.flow.value?.verification_uri).toBe(LINK)

    await vi.advanceTimersByTimeAsync(49)
    expect(api.GET).toHaveBeenCalledTimes(1)
    await vi.advanceTimersByTimeAsync(1)
    expect(store.flow.value?.stage).toBe('finalizing')
    expect(store.error.value).toBeNull()
  })

  it('cancels on the server and drops the link and code', async () => {
    const storage = memoryStorage()
    const api = apiMock()
    api.POST.mockReturnValue(success(waiting(), 201))
    api.DELETE.mockResolvedValue({ response: response(204) })
    api.GET.mockReturnValue(success(flow('cancelled')))
    const { store } = create(api, { storage, pollDelayMs: 60_000 })

    await store.start('@alice:example.org')
    await expect(store.cancel()).resolves.toBe(true)
    expect(api.DELETE).toHaveBeenCalledWith(
      '/v1/accounts/login/browser/{flow_id}',
      {
        params: { path: { flow_id: FLOW_ID } },
        signal: expect.any(AbortSignal),
      },
    )
    expect(store.flow.value).toEqual({
      flow_id: FLOW_ID,
      expected_user_id: '@alice:example.org',
      stage: 'cancelled',
    })
    expect(store.hasStoredFlow()).toBe(false)
  })

  it('takes a flow Axon forgot after the cancellation as cancelled', async () => {
    const api = apiMock()
    api.POST.mockReturnValue(success(waiting(), 201))
    api.DELETE.mockResolvedValue({ response: response(204) })
    api.GET.mockReturnValue(refusal(404, 'not_found', 'unknown flow'))
    const { accounts, store } = create(api, { pollDelayMs: 60_000 })

    await store.start('@alice:example.org')
    await expect(store.cancel()).resolves.toBe(true)
    expect(store.flow.value).toEqual({
      flow_id: FLOW_ID,
      expected_user_id: '@alice:example.org',
      stage: 'cancelled',
    })
    expect(store.error.value).toBeNull()
    expect(accounts.refresh).not.toHaveBeenCalled()
  })

  // Axon answers 204 to a cancellation it can no longer honour, so the
  // request succeeding says nothing about how the sign-in ended.
  it('shows the account when the sign-in finished before the cancellation', async () => {
    const storage = memoryStorage()
    const api = apiMock()
    api.POST.mockReturnValue(success(waiting(), 201))
    api.DELETE.mockResolvedValue({ response: response(204) })
    api.GET.mockReturnValue(success(flow('done', { account_id: 'account-1' })))
    const { accounts, store } = create(api, { storage, pollDelayMs: 60_000 })

    await store.start('@alice:example.org')
    await expect(store.cancel()).resolves.toBe(false)
    expect(store.flow.value?.stage).toBe('done')
    expect(store.flow.value?.account_id).toBe('account-1')
    expect(accounts.refresh).toHaveBeenCalledTimes(1)
    expect(store.hasStoredFlow()).toBe(false)
  })

  it('says it is too late to cancel, and keeps saying so until the flow moves on', async () => {
    vi.useFakeTimers()
    const api = apiMock()
    api.POST.mockReturnValue(success(flow('finalizing'), 201))
    api.DELETE.mockResolvedValue({ response: response(204) })
    api.GET.mockReturnValueOnce(success(flow('finalizing')))
    api.GET.mockReturnValueOnce(success(flow('finalizing')))
    api.GET.mockReturnValue(success(flow('done', { account_id: 'account-1' })))
    const { accounts, store } = create(api, { pollDelayMs: 10 })

    await store.start('@alice:example.org')
    await expect(store.cancel()).resolves.toBe(false)
    expect(store.flow.value?.stage).toBe('finalizing')
    expect(store.error.value).toMatch(/Too late to cancel/)

    // An unchanged poll must not wipe the explanation two seconds later.
    await vi.advanceTimersByTimeAsync(10)
    expect(api.GET).toHaveBeenCalledTimes(2)
    expect(store.error.value).toMatch(/Too late to cancel/)

    await vi.advanceTimersByTimeAsync(10)
    expect(store.flow.value?.stage).toBe('done')
    expect(store.error.value).toBeNull()
    expect(accounts.refresh).toHaveBeenCalledTimes(1)
  })

  it('keeps checking when the outcome of a cancellation cannot be read', async () => {
    vi.useFakeTimers()
    const api = apiMock()
    api.POST.mockReturnValue(success(waiting(), 201))
    api.DELETE.mockResolvedValue({ response: response(204) })
    api.GET.mockRejectedValueOnce(new TypeError('network down'))
    api.GET.mockReturnValue(success(flow('cancelled')))
    const { store } = create(api, {
      pollDelayMs: 60_000,
      transportBackoffMs: 50,
    })

    await store.start('@alice:example.org')
    await expect(store.cancel()).resolves.toBe(false)
    expect(store.error.value).toMatch(/has not confirmed it/)
    expect(store.flow.value?.stage).toBe('waiting_for_authorization')

    await vi.advanceTimersByTimeAsync(50)
    expect(store.flow.value?.stage).toBe('cancelled')
    expect(store.error.value).toBeNull()
  })

  it('keeps the flow when cancellation is refused', async () => {
    const api = apiMock()
    api.POST.mockReturnValue(success(waiting(), 201))
    api.DELETE.mockReturnValue(refusal(500, 'internal', 'storage failed'))
    const { store } = create(api, { pollDelayMs: 60_000 })

    await store.start('@alice:example.org')
    await expect(store.cancel()).resolves.toBe(false)
    expect(store.flow.value?.stage).toBe('waiting_for_authorization')
    expect(store.error.value).toBe('storage failed')
    expect(store.hasStoredFlow()).toBe(true)
  })

  it('does not let a late answer revive a flow that was reset', async () => {
    const api = apiMock()
    let answer: (value: unknown) => void = () => {}
    api.POST.mockReturnValue(
      new Promise((resolve) => {
        answer = resolve
      }),
    )
    const { store } = create(api)

    const started = store.start('@alice:example.org')
    store.reset()
    answer({ data: { data: waiting() }, response: response(201) })

    await expect(started).resolves.toBe(false)
    expect(store.flow.value).toBeNull()
    expect(store.hasStoredFlow()).toBe(false)
  })

  it('cancels the flow it replaces when a new one starts', async () => {
    const api = apiMock()
    api.POST.mockReturnValueOnce(success(waiting(), 201))
    api.POST.mockReturnValueOnce(
      success(flow('starting', { flow_id: FLOW_ID_2 }), 201),
    )
    api.DELETE.mockResolvedValue({ response: response(204) })
    const { store } = create(api, { pollDelayMs: 60_000 })

    await store.start('@alice:example.org')
    await store.start('@alice:example.org')
    expect(api.DELETE).toHaveBeenCalledWith(
      '/v1/accounts/login/browser/{flow_id}',
      {
        params: { path: { flow_id: FLOW_ID } },
        signal: expect.any(AbortSignal),
      },
    )
    expect(store.flow.value?.flow_id).toBe(FLOW_ID_2)
  })

  // After a reload the flow exists only as a stored id. If it cannot be read
  // back, starting again must still release it: Axon allows one sign-in per
  // user, and a forgotten id would hold that slot for half an hour.
  it('cancels a stored flow it could not resume before starting another', async () => {
    const storage = memoryStorage({ [STORAGE_KEY]: FLOW_ID })
    const api = apiMock()
    const order: string[] = []
    api.GET.mockRejectedValue(new TypeError('network down'))
    api.DELETE.mockImplementation(() => {
      order.push('cancel')
      return Promise.resolve({ response: response(204) })
    })
    api.POST.mockImplementation(() => {
      order.push('create')
      return success(flow('starting', { flow_id: FLOW_ID_2 }), 201)
    })
    const { store } = create(api, { storage, pollDelayMs: 60_000 })

    await expect(store.resume()).resolves.toBe(false)
    expect(store.flow.value).toBeNull()
    expect(store.hasStoredFlow()).toBe(true)

    await expect(store.start('@alice:example.org')).resolves.toBe(true)
    expect(api.DELETE).toHaveBeenCalledWith(
      '/v1/accounts/login/browser/{flow_id}',
      {
        params: { path: { flow_id: FLOW_ID } },
        signal: expect.any(AbortSignal),
      },
    )
    expect(order).toEqual(['cancel', 'create'])
    expect(store.flow.value?.flow_id).toBe(FLOW_ID_2)
  })

  it('checks at once on request, but only between polls', async () => {
    const api = apiMock()
    api.POST.mockReturnValue(success(waiting(), 201))
    let answer: (value: unknown) => void = () => {}
    api.GET.mockReturnValue(
      new Promise((resolve) => {
        answer = resolve
      }),
    )
    const { store } = create(api, { pollDelayMs: 60_000 })

    store.checkNow()
    expect(api.GET).not.toHaveBeenCalled()

    await store.start('@alice:example.org')
    store.checkNow()
    expect(api.GET).toHaveBeenCalledTimes(1)
    // A second request would abort the first and throw its answer away.
    store.checkNow()
    expect(api.GET).toHaveBeenCalledTimes(1)

    answer({
      data: { data: flow('done', { account_id: 'account-1' }) },
      response: response(),
    })
    await vi.waitFor(() => expect(store.flow.value?.stage).toBe('done'))
    store.checkNow()
    expect(api.GET).toHaveBeenCalledTimes(1)
  })
})
