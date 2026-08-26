import { computed, signal } from '@preact/signals'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { ApiClient } from '../api/client'
import { memoryStorage } from '../test/memory-storage'
import type { AccountsStore } from './accounts'
import {
  createMatrixOAuthQrStore,
  type MatrixOAuthQrFlow,
  type MatrixOAuthQrStore,
} from './matrix-oauth-qr'

const FLOW_ID = '10000000-0000-4000-8000-000000000001'
const FLOW_ID_2 = '20000000-0000-4000-8000-000000000002'

function flow(
  stage: MatrixOAuthQrFlow['stage'] = 'starting',
  overrides: Partial<MatrixOAuthQrFlow> = {},
): MatrixOAuthQrFlow {
  return {
    flow_id: FLOW_ID,
    expected_user_id: '@alice:example.org',
    presentation: 'display',
    stage,
    ...overrides,
  }
}

function response(status = 200): Response {
  return new Response(null, { status })
}

function success(data: MatrixOAuthQrFlow, status = 200) {
  return Promise.resolve({ data: { data }, response: response(status) })
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
    remove: vi.fn(),
  }
}

const stores: MatrixOAuthQrStore[] = []
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
  const store = createMatrixOAuthQrStore(
    api as unknown as ApiClient,
    accounts,
    options,
  )
  stores.push(store)
  return { accounts, store }
}

describe('MatrixOAuthQrStore', () => {
  it('starts either presentation, persists only the flow id, and polls serially', async () => {
    vi.useFakeTimers()
    const storage = memoryStorage()
    const api = apiMock()
    api.POST.mockReturnValue(
      success(flow('starting', { presentation: 'scan' }), 201),
    )
    api.GET.mockReturnValue(
      success(
        flow('check_code_to_display', {
          presentation: 'scan',
          check_code: '42',
        }),
      ),
    )
    const { store } = create(api, { storage, pollDelayMs: 1 })

    await expect(store.start('@alice:example.org', 'scan')).resolves.toBe(true)
    expect(api.POST).toHaveBeenCalledWith('/v1/accounts/login/qr', {
      body: {
        expected_user_id: '@alice:example.org',
        presentation: 'scan',
      },
      signal: expect.any(AbortSignal),
    })
    expect(storage.length).toBe(1)
    expect(storage.getItem('axon.matrix-oauth-qr.flow-id')).toBe(FLOW_ID)

    await vi.advanceTimersByTimeAsync(1)
    expect(api.GET).toHaveBeenCalledTimes(1)
    expect(store.flow.value?.check_code).toBe('42')
  })

  it('resumes a flow after reload and clears expired flow ids', async () => {
    const storage = memoryStorage({
      'axon.matrix-oauth-qr.flow-id': FLOW_ID,
    })
    const api = apiMock()
    api.GET.mockResolvedValue({
      error: { error: { code: 'not_found', message: 'unknown flow' } },
      response: response(404),
    })
    const { store } = create(api, { storage })

    await expect(store.resume()).resolves.toBe(false)
    expect(store.flow.value).toBeNull()
    expect(storage.getItem('axon.matrix-oauth-qr.flow-id')).toBeNull()
    expect(store.error.value).toMatch(/expired/i)
  })

  it('submits scan and check-code inputs only to their typed routes', async () => {
    const api = apiMock()
    api.POST.mockReturnValueOnce(
      success(flow('starting', { presentation: 'scan' }), 201),
    )
      .mockReturnValueOnce(
        success(
          flow('check_code_to_display', {
            presentation: 'scan',
            check_code: '07',
          }),
        ),
      )
      .mockReturnValueOnce(
        success(flow('starting', { flow_id: FLOW_ID_2 }), 201),
      )
      .mockReturnValueOnce(
        success(flow('waiting_for_authorization', { flow_id: FLOW_ID_2 })),
      )
    api.DELETE.mockResolvedValue({ response: response(204) })
    const { store } = create(api)

    await store.start('@alice:example.org', 'scan')
    await expect(store.submitScan('AAH_')).resolves.toBe(true)
    expect(api.POST).toHaveBeenNthCalledWith(
      2,
      '/v1/accounts/login/qr/{flow_id}/scan',
      expect.objectContaining({ body: { qr_code_data: 'AAH_' } }),
    )

    await store.start('@alice:example.org', 'display')
    await expect(store.submitCheckCode('07')).resolves.toBe(true)
    expect(api.POST).toHaveBeenNthCalledWith(
      4,
      '/v1/accounts/login/qr/{flow_id}/check-code',
      expect.objectContaining({ body: { check_code: '07' } }),
    )
  })

  it('reconciles an ambiguous one-shot input through GET', async () => {
    const api = apiMock()
    api.POST.mockReturnValueOnce(
      success(flow('starting', { presentation: 'scan' }), 201),
    ).mockRejectedValueOnce(new TypeError('connection reset'))
    api.GET.mockReturnValue(
      success(
        flow('check_code_to_display', {
          presentation: 'scan',
          check_code: '81',
        }),
      ),
    )
    const { store } = create(api)

    await store.start('@alice:example.org', 'scan')
    await expect(store.submitScan('AQID')).resolves.toBe(true)
    expect(api.GET).toHaveBeenCalledTimes(1)
    expect(store.flow.value?.stage).toBe('check_code_to_display')
    expect(store.flow.value?.check_code).toBe('81')
  })

  it('keeps a rejected scan visible until retry or stage advancement', async () => {
    vi.useFakeTimers()
    const api = apiMock()
    api.POST.mockReturnValueOnce(
      success(flow('starting', { presentation: 'scan' }), 201),
    ).mockResolvedValueOnce({
      error: {
        error: {
          code: 'invalid_request',
          message: 'QR payload is not compatible with this flow',
        },
      },
      response: response(400),
    })
    api.GET.mockReturnValueOnce(
      success(flow('starting', { presentation: 'scan' })),
    ).mockReturnValueOnce(
      success(
        flow('check_code_to_display', {
          presentation: 'scan',
          check_code: '42',
        }),
      ),
    )
    const { store } = create(api, { pollDelayMs: 10 })

    await store.start('@alice:example.org', 'scan')
    await expect(store.submitScan('AQID')).resolves.toBe(false)
    expect(store.error.value).toMatch(/not compatible/i)

    await vi.advanceTimersByTimeAsync(10)
    expect(store.flow.value?.stage).toBe('starting')
    expect(store.error.value).toMatch(/not compatible/i)

    await vi.advanceTimersByTimeAsync(10)
    expect(store.flow.value?.stage).toBe('check_code_to_display')
    expect(store.error.value).toBeNull()
  })

  it('backs transport-failed polls off and refreshes accounts once on done', async () => {
    vi.useFakeTimers()
    const api = apiMock()
    api.POST.mockReturnValue(success(flow('syncing_secrets'), 201))
    api.GET.mockRejectedValueOnce(new TypeError('offline')).mockReturnValueOnce(
      success(
        flow('done', {
          account_id: '30000000-0000-4000-8000-000000000003',
        }),
      ),
    )
    const { accounts, store } = create(api, {
      pollDelayMs: 10,
      transportBackoffMs: 50,
    })

    await store.start('@alice:example.org', 'display')
    await vi.advanceTimersByTimeAsync(10)
    expect(api.GET).toHaveBeenCalledTimes(1)
    await vi.advanceTimersByTimeAsync(49)
    expect(api.GET).toHaveBeenCalledTimes(1)
    await vi.advanceTimersByTimeAsync(1)
    expect(store.flow.value?.stage).toBe('done')
    expect(accounts.refresh).toHaveBeenCalledTimes(1)
    await vi.advanceTimersByTimeAsync(100)
    expect(accounts.refresh).toHaveBeenCalledTimes(1)
  })

  it('times out requests and leaves a recoverable error', async () => {
    vi.useFakeTimers()
    const api = apiMock()
    api.POST.mockImplementation(
      (_path: string, options: { signal: AbortSignal }) =>
        new Promise((_resolve, reject) => {
          options.signal.addEventListener('abort', () =>
            reject(new DOMException('aborted', 'AbortError')),
          )
        }),
    )
    const { store } = create(api, { requestTimeoutMs: 15 })

    const started = store.start('@alice:example.org', 'display')
    await vi.advanceTimersByTimeAsync(15)
    await expect(started).resolves.toBe(false)
    expect(store.operation.value).toBe('idle')
    expect(store.error.value).toMatch(/timed out/i)
  })

  it('generation-fences a late response after replacement', async () => {
    let resolveFirst!: (value: unknown) => void
    const api = apiMock()
    api.POST.mockReturnValueOnce(
      new Promise((resolve) => (resolveFirst = resolve)),
    ).mockReturnValueOnce(
      success(
        flow('qr_ready', {
          flow_id: FLOW_ID_2,
          expected_user_id: '@bob:example.org',
          qr_code_data: 'Ag',
        }),
        201,
      ),
    )
    const { store } = create(api)

    const first = store.start('@alice:example.org', 'display')
    await store.start('@bob:example.org', 'display')
    resolveFirst({
      data: { data: flow('qr_ready', { qr_code_data: 'AQ' }) },
      response: response(201),
    })
    await expect(first).resolves.toBe(false)
    expect(store.flow.value?.flow_id).toBe(FLOW_ID_2)
    expect(store.flow.value?.expected_user_id).toBe('@bob:example.org')
  })

  it('keeps an unconfirmed cancellation available to retry', async () => {
    const api = apiMock()
    api.POST.mockReturnValue(
      success(flow('qr_ready', { qr_code_data: 'AQ' }), 201),
    )
    api.DELETE.mockRejectedValue(new TypeError('offline'))
    const { store } = create(api)

    await store.start('@alice:example.org', 'display')
    await expect(store.cancel()).resolves.toBe(false)
    expect(store.flow.value?.stage).toBe('qr_ready')
    expect(store.error.value).toMatch(/not confirmed/i)
  })

  it('maps stable terminal failure codes to actionable text', async () => {
    const api = apiMock()
    api.POST.mockReturnValue(
      success(flow('failed', { error_code: 'unsupported' }), 201),
    )
    const { store } = create(api)

    await store.start('@alice:example.org', 'display')
    expect(store.error.value).toMatch(/homeserver does not support/i)
  })
})
