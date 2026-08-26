import {
  computed,
  signal,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import { apiErrorCode, apiErrorMessage, type ApiClient } from '../api/client'
import type { components } from '../api/schema'
import type { AccountsStore } from './accounts'

export type MatrixOAuthQrFlow = components['schemas']['MatrixOAuthQrFlowDto']
export type MatrixOAuthQrPresentation =
  components['schemas']['MatrixOAuthQrPresentation']

export type MatrixOAuthQrOperation =
  | 'idle'
  | 'starting'
  | 'resuming'
  | 'polling'
  | 'submitting_scan'
  | 'submitting_check_code'
  | 'cancelling'

export interface MatrixOAuthQrStore {
  flow: ReadonlySignal<MatrixOAuthQrFlow | null>
  operation: ReadonlySignal<MatrixOAuthQrOperation>
  error: Signal<string | null>
  start(
    userId: string,
    presentation: MatrixOAuthQrPresentation,
  ): Promise<boolean>
  submitScan(qrCodeData: string): Promise<boolean>
  submitCheckCode(checkCode: string): Promise<boolean>
  cancel(): Promise<boolean>
  resume(): Promise<boolean>
  reset(): void
}

interface MatrixOAuthQrStoreOptions {
  storage?: Storage
  requestTimeoutMs?: number
  pollDelayMs?: number
  transportBackoffMs?: number
}

const ACTIVE_FLOW_KEY = 'axon.matrix-oauth-qr.flow-id'
const REQUEST_TIMEOUT_MS = 15_000
const POLL_DELAY_MS = 1_000
const TRANSPORT_BACKOFF_MS = 5_000

const TERMINAL_STAGES = new Set<MatrixOAuthQrFlow['stage']>([
  'done',
  'failed',
  'cancelled',
])

const FAILURE_MESSAGES: Record<string, string> = {
  cancelled: 'The QR sign-in was cancelled.',
  timeout: 'The QR sign-in timed out. Start a new flow and try again.',
  unsupported:
    'This homeserver does not support sign in and verification with QR.',
  invalid_qr:
    'That QR code cannot be used for this sign-in. Scan a fresh code.',
  invalid_check_code:
    'The check code was rejected. Start a new flow and compare the new code carefully.',
  rendezvous_expired:
    'The QR rendezvous expired. Start a new flow on both devices.',
  user_mismatch:
    'The trusted device authorized a different Matrix user. Start again with the expected user ID.',
  upstream:
    'The homeserver or authorization service could not complete QR sign-in. Try again.',
  conflict:
    'This account is already active or another account operation is in progress.',
  device_not_verified:
    'Sign-in completed, but Axon could not confirm that its new device is verified.',
  internal:
    'Axon could not complete QR sign-in. Try again or check the server logs.',
}

function terminal(flow: MatrixOAuthQrFlow): boolean {
  return TERMINAL_STAGES.has(flow.stage)
}

function failureMessage(code: string | null | undefined): string {
  return code === null || code === undefined
    ? 'QR sign-in failed. Start a new flow and try again.'
    : (FAILURE_MESSAGES[code] ??
        'QR sign-in failed. Start a new flow and try again.')
}

function transportMessage(cause: unknown): string {
  if (cause instanceof DOMException && cause.name === 'AbortError') {
    return 'The QR request timed out. Axon will check the flow before allowing a retry.'
  }
  return cause instanceof Error
    ? `Could not reach Axon: ${cause.message}`
    : 'Could not reach Axon. Check the connection and try again.'
}

/**
 * Replayable Matrix OAuth QR acquisition state (ADR 0097 PR 4).
 *
 * Only the opaque flow id reaches session storage. Presentation data is held in
 * memory, every network call is bounded, and generation checks prevent a late
 * response from reviving a cancelled or replaced flow.
 */
export function createMatrixOAuthQrStore(
  api: ApiClient,
  accounts: AccountsStore,
  options: MatrixOAuthQrStoreOptions = {},
): MatrixOAuthQrStore {
  const storage = options.storage ?? window.sessionStorage
  const requestTimeoutMs = options.requestTimeoutMs ?? REQUEST_TIMEOUT_MS
  const pollDelayMs = options.pollDelayMs ?? POLL_DELAY_MS
  const transportBackoffMs = options.transportBackoffMs ?? TRANSPORT_BACKOFF_MS
  const flow = signal<MatrixOAuthQrFlow | null>(null)
  const operation = signal<MatrixOAuthQrOperation>('idle')
  const error = signal<string | null>(null)
  let generation = 0
  let request: AbortController | null = null
  let pollTimer: ReturnType<typeof setTimeout> | null = null
  const refreshed = new Set<string>()

  function clearPoll(): void {
    if (pollTimer !== null) {
      clearTimeout(pollTimer)
      pollTimer = null
    }
  }

  function beginRequest(): {
    controller: AbortController
    timedOut: () => boolean
    finish: () => void
  } {
    request?.abort()
    const controller = new AbortController()
    request = controller
    let timeout = false
    const timer = setTimeout(() => {
      timeout = true
      controller.abort()
    }, requestTimeoutMs)
    return {
      controller,
      timedOut: () => timeout,
      finish: () => {
        clearTimeout(timer)
        if (request === controller) {
          request = null
        }
      },
    }
  }

  function schedulePoll(owner: number, delay = pollDelayMs): void {
    clearPoll()
    if (
      owner !== generation ||
      (flow.value === null && storage.getItem(ACTIVE_FLOW_KEY) === null) ||
      (flow.value !== null && terminal(flow.value))
    ) {
      return
    }
    pollTimer = setTimeout(() => {
      pollTimer = null
      void fetchCurrent(owner, 'polling')
    }, delay)
  }

  function applyFlow(next: MatrixOAuthQrFlow, owner: number): void {
    if (owner !== generation) {
      return
    }
    flow.value = next
    error.value =
      next.stage === 'failed' ? failureMessage(next.error_code) : null
    if (terminal(next)) {
      clearPoll()
      storage.removeItem(ACTIVE_FLOW_KEY)
      operation.value = 'idle'
      if (next.stage === 'done' && !refreshed.has(next.flow_id)) {
        refreshed.add(next.flow_id)
        void accounts.refresh()
      }
      return
    }
    storage.setItem(ACTIVE_FLOW_KEY, next.flow_id)
    operation.value = 'idle'
    schedulePoll(owner)
  }

  async function fetchCurrent(
    owner: number,
    kind: 'polling' | 'resuming',
  ): Promise<boolean> {
    if (owner !== generation) {
      return false
    }
    const flowId = flow.value?.flow_id ?? storage.getItem(ACTIVE_FLOW_KEY)
    if (flowId === null || flowId === '') {
      operation.value = 'idle'
      return false
    }
    operation.value = kind
    const active = beginRequest()
    try {
      const result = await api.GET('/v1/accounts/login/qr/{flow_id}', {
        params: { path: { flow_id: flowId } },
        signal: active.controller.signal,
      })
      if (owner !== generation || active.controller.signal.aborted) {
        return false
      }
      if (result.error !== undefined) {
        if (result.response.status === 404) {
          storage.removeItem(ACTIVE_FLOW_KEY)
          flow.value = null
          error.value = 'This QR sign-in expired. Start a new flow to continue.'
        } else {
          error.value = apiErrorMessage(result.error)
          schedulePoll(owner, transportBackoffMs)
        }
        operation.value = 'idle'
        return false
      }
      applyFlow(result.data.data, owner)
      return true
    } catch (cause) {
      if (
        owner !== generation ||
        (!active.timedOut() && active.controller.signal.aborted)
      ) {
        return false
      }
      error.value = transportMessage(cause)
      operation.value = 'idle'
      schedulePoll(owner, transportBackoffMs)
      return false
    } finally {
      active.finish()
    }
  }

  async function reconcileAmbiguous(owner: number): Promise<boolean> {
    const reconciled = await fetchCurrent(owner, 'polling')
    if (!reconciled && owner === generation) {
      error.value =
        'The request outcome is unknown. Axon is checking the flow before you retry.'
    }
    return reconciled
  }

  async function submitInput(
    kind: 'submitting_scan' | 'submitting_check_code',
    call: (
      flowId: string,
      signal: AbortSignal,
    ) => ReturnType<ApiClient['POST']>,
  ): Promise<boolean> {
    const current = flow.value
    if (current === null || terminal(current)) {
      return false
    }
    const owner = generation
    clearPoll()
    operation.value = kind
    error.value = null
    const active = beginRequest()
    try {
      const result = await call(current.flow_id, active.controller.signal)
      if (owner !== generation || active.controller.signal.aborted) {
        return false
      }
      if (result.error !== undefined) {
        error.value = apiErrorMessage(result.error)
        operation.value = 'idle'
        schedulePoll(owner)
        return false
      }
      applyFlow(result.data.data as MatrixOAuthQrFlow, owner)
      return true
    } catch {
      if (
        owner !== generation ||
        (!active.timedOut() && active.controller.signal.aborted)
      ) {
        return false
      }
      operation.value = 'idle'
      return reconcileAmbiguous(owner)
    } finally {
      active.finish()
    }
  }

  return {
    flow: computed(() => flow.value),
    operation: computed(() => operation.value),
    error,

    start: async (userId, presentation) => {
      const previous = flow.value
      generation += 1
      const owner = generation
      clearPoll()
      request?.abort()
      flow.value = null
      error.value = null
      operation.value = 'starting'
      storage.removeItem(ACTIVE_FLOW_KEY)
      if (previous !== null && !terminal(previous)) {
        const controller = new AbortController()
        const timer = setTimeout(() => controller.abort(), requestTimeoutMs)
        void api
          .DELETE('/v1/accounts/login/qr/{flow_id}', {
            params: { path: { flow_id: previous.flow_id } },
            signal: controller.signal,
          })
          .catch(() => {})
          .finally(() => clearTimeout(timer))
      }
      const active = beginRequest()
      try {
        const result = await api.POST('/v1/accounts/login/qr', {
          body: {
            expected_user_id: userId,
            presentation,
          },
          signal: active.controller.signal,
        })
        if (owner !== generation || active.controller.signal.aborted) {
          return false
        }
        if (result.error !== undefined) {
          const code = apiErrorCode(result.error)
          error.value =
            code !== null && code in FAILURE_MESSAGES
              ? failureMessage(code)
              : apiErrorMessage(result.error)
          operation.value = 'idle'
          return false
        }
        applyFlow(result.data.data, owner)
        return true
      } catch (cause) {
        if (
          owner !== generation ||
          (!active.timedOut() && active.controller.signal.aborted)
        ) {
          return false
        }
        error.value = transportMessage(cause)
        operation.value = 'idle'
        return false
      } finally {
        active.finish()
      }
    },

    submitScan: (qrCodeData) =>
      submitInput('submitting_scan', (flowId, requestSignal) =>
        api.POST('/v1/accounts/login/qr/{flow_id}/scan', {
          params: { path: { flow_id: flowId } },
          body: { qr_code_data: qrCodeData },
          signal: requestSignal,
        }),
      ),

    submitCheckCode: (checkCode) =>
      submitInput('submitting_check_code', (flowId, requestSignal) =>
        api.POST('/v1/accounts/login/qr/{flow_id}/check-code', {
          params: { path: { flow_id: flowId } },
          body: { check_code: checkCode },
          signal: requestSignal,
        }),
      ),

    cancel: async () => {
      const current = flow.value
      if (current === null) {
        return true
      }
      const owner = generation
      clearPoll()
      operation.value = 'cancelling'
      error.value = null
      const active = beginRequest()
      try {
        const result = await api.DELETE('/v1/accounts/login/qr/{flow_id}', {
          params: { path: { flow_id: current.flow_id } },
          signal: active.controller.signal,
        })
        if (owner !== generation || active.controller.signal.aborted) {
          return false
        }
        if (result.error !== undefined) {
          error.value = apiErrorMessage(result.error)
          operation.value = 'idle'
          schedulePoll(owner)
          return false
        }
        generation += 1
        flow.value = { ...current, stage: 'cancelled' }
        storage.removeItem(ACTIVE_FLOW_KEY)
        operation.value = 'idle'
        return true
      } catch (cause) {
        if (
          owner !== generation ||
          (!active.timedOut() && active.controller.signal.aborted)
        ) {
          return false
        }
        error.value = `${transportMessage(cause)} Cancellation was not confirmed; you can retry it.`
        operation.value = 'idle'
        schedulePoll(owner)
        return false
      } finally {
        active.finish()
      }
    },

    resume: () => {
      if (flow.value !== null || storage.getItem(ACTIVE_FLOW_KEY) === null) {
        return Promise.resolve(false)
      }
      generation += 1
      return fetchCurrent(generation, 'resuming')
    },

    reset: () => {
      generation += 1
      clearPoll()
      request?.abort()
      request = null
      flow.value = null
      operation.value = 'idle'
      error.value = null
      storage.removeItem(ACTIVE_FLOW_KEY)
    },
  }
}
