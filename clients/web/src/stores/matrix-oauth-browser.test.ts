import { computed, signal } from '@preact/signals'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { ApiClient } from '../api/client'
import { memoryStorage } from '../test/memory-storage'
import type { AccountsStore } from './accounts'
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

function accountsMock(): AccountsStore {
  return {
    accounts: computed(() => []),
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
  const accounts = accountsMock()
  const store = createMatrixOAuthBrowserStore(
    api as unknown as ApiClient,
    accounts,
    options,
  )
  stores.push(store)
  return { accounts, store }
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
    const { store } = create(api, { storage })

    await expect(store.resume()).resolves.toBe(false)
    expect(store.flow.value).toBeNull()
    expect(store.hasStoredFlow()).toBe(false)
    expect(store.error.value).toMatch(/no longer open/)
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
