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
export type MatrixOAuthQrGrantFlow =
  components['schemas']['MatrixOAuthQrGrantFlowDto']
export type MatrixOAuthQrPresentation =
  components['schemas']['MatrixOAuthQrPresentation']

type MatrixOAuthQrFlowState = Pick<
  MatrixOAuthQrFlow,
  | 'flow_id'
  | 'presentation'
  | 'stage'
  | 'qr_code_data'
  | 'check_code'
  | 'verification_uri'
  | 'error_code'
>

export type MatrixOAuthQrOperation =
  | 'idle'
  | 'starting'
  | 'resuming'
  | 'polling'
  | 'submitting_scan'
  | 'submitting_check_code'
  | 'refreshing_accounts'
  | 'cancelling'

interface MatrixOAuthQrFlowControls<T extends MatrixOAuthQrFlowState> {
  flow: ReadonlySignal<T | null>
  operation: ReadonlySignal<MatrixOAuthQrOperation>
  error: Signal<string | null>
  submitScan(qrCodeData: string): Promise<boolean>
  submitCheckCode(checkCode: string): Promise<boolean>
  cancel(): Promise<boolean>
  reset(): void
}

export interface MatrixOAuthQrStore extends MatrixOAuthQrFlowControls<MatrixOAuthQrFlow> {
  start(
    userId: string,
    presentation: MatrixOAuthQrPresentation,
  ): Promise<boolean>
  resume(): Promise<boolean>
}

export interface MatrixOAuthQrGrantStore extends MatrixOAuthQrFlowControls<MatrixOAuthQrGrantFlow> {
  start(
    accountId: string,
    presentation: MatrixOAuthQrPresentation,
  ): Promise<boolean>
  /** Probe account scopes serially because only the opaque flow id is stored. */
  resume(accountIds: readonly string[]): Promise<boolean>
}

interface MatrixOAuthQrStoreOptions {
  storage?: Storage
  requestTimeoutMs?: number
  pollDelayMs?: number
  transportBackoffMs?: number
}

interface ApiResult<T> {
  data?: { data: T }
  error?: unknown
  response: Response
}

interface FlowRoutes<T extends MatrixOAuthQrFlowState> {
  storageKey: string
  noun: string
  scopeOf(flow: T): string | null
  get(
    scope: string | null,
    flowId: string,
    signal: AbortSignal,
  ): Promise<ApiResult<T>>
  scan(
    scope: string | null,
    flowId: string,
    qrCodeData: string,
    signal: AbortSignal,
  ): Promise<ApiResult<T>>
  checkCode(
    scope: string | null,
    flowId: string,
    checkCode: string,
    signal: AbortSignal,
  ): Promise<ApiResult<T>>
  cancel(
    scope: string | null,
    flowId: string,
    signal: AbortSignal,
  ): Promise<ApiResult<never>>
  failureFallback: string
  knownFailureMessage(code: string | null | undefined): string | undefined
  onDone?(flow: T): Promise<void>
}

interface MatrixOAuthQrFlowCore<
  T extends MatrixOAuthQrFlowState,
> extends MatrixOAuthQrFlowControls<T> {
  start(
    scope: string | null,
    create: (signal: AbortSignal) => Promise<ApiResult<T>>,
  ): Promise<boolean>
  resume(scopes: readonly (string | null)[]): Promise<boolean>
}

const ACQUIRE_ACTIVE_FLOW_KEY = 'axon.matrix-oauth-qr.flow-id'
const GRANT_ACTIVE_FLOW_KEY = 'axon.matrix-oauth-qr-grant.flow-id'
const REQUEST_TIMEOUT_MS = 15_000
const POLL_DELAY_MS = 1_000
const TRANSPORT_BACKOFF_MS = 5_000

const TERMINAL_STAGES = new Set<MatrixOAuthQrFlowState['stage']>([
  'done',
  'failed',
  'cancelled',
])

const ACQUIRE_FAILURE_MESSAGES: Record<string, string> = {
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

const GRANT_FAILURE_MESSAGES: Record<string, string> = {
  cancelled: 'Device authorization was cancelled.',
  timeout: 'Device authorization timed out. Start again on both devices.',
  unsupported:
    'This homeserver does not support authorizing another device with QR.',
  invalid_qr:
    'That QR code cannot be used to authorize this device. Scan a fresh code from the new device.',
  invalid_check_code:
    'The check code was rejected. Start again and compare the new code carefully.',
  rendezvous_expired:
    'The QR rendezvous expired. Start a new authorization on both devices.',
  device_conflict:
    'The new Matrix device conflicts with an existing device. Start again from a fresh sign-in on the new client.',
  device_not_found:
    'The new device was not found for this Matrix account. Confirm that the new client is signing in to the expected account and that its device was provisioned, then start again.',
  trust_lost:
    'Axon can no longer prove that this account is trusted. Restore verification before authorizing another device.',
  secrets_unavailable:
    'Axon cannot export the encryption secrets required to provision the new device.',
  upstream:
    'The homeserver or authorization service could not complete device authorization. Try again.',
  internal:
    'Axon could not authorize the new device. Try again or check the server logs.',
}

export function isTerminalMatrixOAuthQrFlow(
  flow: MatrixOAuthQrFlowState,
): boolean {
  return TERMINAL_STAGES.has(flow.stage)
}

function knownFailureMessage(
  messages: Record<string, string>,
  code: string | null | undefined,
): string | undefined {
  return code === null || code === undefined ? undefined : messages[code]
}

function transportMessage(cause: unknown, noun: string): string {
  if (cause instanceof DOMException && cause.name === 'AbortError') {
    return `The ${noun} request timed out. Axon will check the flow before allowing a retry.`
  }
  return cause instanceof Error
    ? `Could not reach Axon: ${cause.message}`
    : 'Could not reach Axon. Check the connection and try again.'
}

/**
 * Shared replayable QR-flow lifecycle for both directions of ADR 0097.
 *
 * Only the opaque flow id reaches session storage. Presentation data is held in
 * memory, every network call is bounded, and generation checks prevent a late
 * response from reviving a cancelled or replaced flow.
 */
function createMatrixOAuthQrFlowCore<T extends MatrixOAuthQrFlowState>(
  routes: FlowRoutes<T>,
  options: MatrixOAuthQrStoreOptions,
): MatrixOAuthQrFlowCore<T> {
  const storage = options.storage ?? window.sessionStorage
  const requestTimeoutMs = options.requestTimeoutMs ?? REQUEST_TIMEOUT_MS
  const pollDelayMs = options.pollDelayMs ?? POLL_DELAY_MS
  const transportBackoffMs = options.transportBackoffMs ?? TRANSPORT_BACKOFF_MS
  const flow = signal<T | null>(null)
  const operation = signal<MatrixOAuthQrOperation>('idle')
  const error = signal<string | null>(null)
  let generation = 0
  let request: AbortController | null = null
  let pollTimer: ReturnType<typeof setTimeout> | null = null
  let routeScope: string | null = null
  const completed = new Set<string>()
  let retainedErrorAt: Pick<T, 'flow_id' | 'stage'> | null = null

  function clearRetainedError(): void {
    retainedErrorAt = null
  }

  function retainError(message: string, current: T | null = flow.value): void {
    error.value = message
    retainedErrorAt =
      current === null
        ? null
        : { flow_id: current.flow_id, stage: current.stage }
  }

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

  function schedulePoll(
    owner: number,
    delay = pollDelayMs,
    scopes: readonly (string | null)[] = [routeScope],
  ): void {
    clearPoll()
    if (
      owner !== generation ||
      (flow.value === null && storage.getItem(routes.storageKey) === null) ||
      (flow.value !== null && isTerminalMatrixOAuthQrFlow(flow.value))
    ) {
      return
    }
    const retryScopes = [...scopes]
    pollTimer = setTimeout(() => {
      pollTimer = null
      void fetchCurrent(owner, 'polling', retryScopes)
    }, delay)
  }

  async function applyFlow(next: T, owner: number): Promise<void> {
    if (owner !== generation) {
      return
    }
    routeScope = routes.scopeOf(next)
    const runCompletion =
      next.stage === 'done' &&
      routes.onDone !== undefined &&
      !completed.has(next.flow_id)
    if (runCompletion) {
      completed.add(next.flow_id)
      operation.value = 'refreshing_accounts'
    }
    const retainCurrentError =
      retainedErrorAt?.flow_id === next.flow_id &&
      retainedErrorAt.stage === next.stage
    flow.value = next
    if (next.stage === 'failed') {
      clearRetainedError()
      error.value =
        routes.knownFailureMessage(next.error_code) ?? routes.failureFallback
    } else if (!retainCurrentError) {
      clearRetainedError()
      error.value = null
    }
    if (isTerminalMatrixOAuthQrFlow(next)) {
      clearPoll()
      storage.removeItem(routes.storageKey)
      if (runCompletion) {
        await routes.onDone?.(next)
        if (owner !== generation) {
          return
        }
      }
      operation.value = 'idle'
      return
    }
    storage.setItem(routes.storageKey, next.flow_id)
    operation.value = 'idle'
    schedulePoll(owner)
  }

  async function fetchCurrent(
    owner: number,
    kind: 'polling' | 'resuming',
    scopes: readonly (string | null)[],
  ): Promise<boolean> {
    if (owner !== generation) {
      return false
    }
    const flowId = flow.value?.flow_id ?? storage.getItem(routes.storageKey)
    if (flowId === null || flowId === '' || scopes.length === 0) {
      operation.value = 'idle'
      return false
    }
    operation.value = kind
    const active = beginRequest()
    let retryScopes = scopes
    try {
      for (let index = 0; index < scopes.length; index += 1) {
        const scope = scopes[index]
        retryScopes = scopes.slice(index)
        const result = await routes.get(scope, flowId, active.controller.signal)
        if (owner !== generation || active.controller.signal.aborted) {
          return false
        }
        if (result.error === undefined && result.data !== undefined) {
          routeScope = scope
          await applyFlow(result.data.data, owner)
          return true
        }
        if (result.response.status === 404) {
          continue
        }
        clearRetainedError()
        error.value = apiErrorMessage(result.error)
        operation.value = 'idle'
        schedulePoll(owner, transportBackoffMs, retryScopes)
        return false
      }
      storage.removeItem(routes.storageKey)
      flow.value = null
      error.value = `This ${routes.noun} flow expired. Start a new flow to continue.`
      operation.value = 'idle'
      return false
    } catch (cause) {
      if (
        owner !== generation ||
        (!active.timedOut() && active.controller.signal.aborted)
      ) {
        return false
      }
      clearRetainedError()
      error.value = transportMessage(cause, routes.noun)
      operation.value = 'idle'
      schedulePoll(owner, transportBackoffMs, retryScopes)
      return false
    } finally {
      active.finish()
    }
  }

  async function reconcileAmbiguous(owner: number): Promise<boolean> {
    const reconciled = await fetchCurrent(owner, 'polling', [routeScope])
    if (!reconciled && owner === generation) {
      retainError(
        'The request outcome is unknown. Axon is checking the flow before you retry.',
      )
    }
    return reconciled
  }

  async function submitInput(
    kind: 'submitting_scan' | 'submitting_check_code',
    call: (flowId: string, signal: AbortSignal) => Promise<ApiResult<T>>,
  ): Promise<boolean> {
    const current = flow.value
    if (current === null || isTerminalMatrixOAuthQrFlow(current)) {
      return false
    }
    const owner = generation
    clearPoll()
    operation.value = kind
    clearRetainedError()
    error.value = null
    const active = beginRequest()
    try {
      const result = await call(current.flow_id, active.controller.signal)
      if (owner !== generation || active.controller.signal.aborted) {
        return false
      }
      if (result.error !== undefined || result.data === undefined) {
        retainError(apiErrorMessage(result.error), current)
        operation.value = 'idle'
        schedulePoll(owner)
        return false
      }
      await applyFlow(result.data.data, owner)
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

    start: async (scope, create) => {
      const previous = flow.value
      const previousScope =
        previous === null ? routeScope : routes.scopeOf(previous)
      generation += 1
      const owner = generation
      clearPoll()
      request?.abort()
      flow.value = null
      routeScope = scope
      clearRetainedError()
      error.value = null
      operation.value = 'starting'
      storage.removeItem(routes.storageKey)
      if (previous !== null && !isTerminalMatrixOAuthQrFlow(previous)) {
        const controller = new AbortController()
        const timer = setTimeout(() => controller.abort(), requestTimeoutMs)
        void routes
          .cancel(previousScope, previous.flow_id, controller.signal)
          .catch(() => {})
          .finally(() => clearTimeout(timer))
      }
      const active = beginRequest()
      try {
        const result = await create(active.controller.signal)
        if (owner !== generation || active.controller.signal.aborted) {
          return false
        }
        if (result.error !== undefined || result.data === undefined) {
          const code = apiErrorCode(result.error)
          error.value =
            code === null
              ? apiErrorMessage(result.error)
              : (routes.knownFailureMessage(code) ??
                apiErrorMessage(result.error))
          operation.value = 'idle'
          return false
        }
        await applyFlow(result.data.data, owner)
        return true
      } catch (cause) {
        if (
          owner !== generation ||
          (!active.timedOut() && active.controller.signal.aborted)
        ) {
          return false
        }
        error.value = transportMessage(cause, routes.noun)
        operation.value = 'idle'
        return false
      } finally {
        active.finish()
      }
    },

    submitScan: (qrCodeData) =>
      submitInput('submitting_scan', (flowId, requestSignal) =>
        routes.scan(routeScope, flowId, qrCodeData, requestSignal),
      ),

    submitCheckCode: (checkCode) =>
      submitInput('submitting_check_code', (flowId, requestSignal) =>
        routes.checkCode(routeScope, flowId, checkCode, requestSignal),
      ),

    cancel: async () => {
      const current = flow.value
      if (current === null) {
        return true
      }
      const owner = generation
      clearPoll()
      operation.value = 'cancelling'
      clearRetainedError()
      error.value = null
      const active = beginRequest()
      try {
        const result = await routes.cancel(
          routeScope,
          current.flow_id,
          active.controller.signal,
        )
        if (owner !== generation || active.controller.signal.aborted) {
          return false
        }
        if (result.error !== undefined) {
          retainError(apiErrorMessage(result.error), current)
          operation.value = 'idle'
          schedulePoll(owner)
          return false
        }
        generation += 1
        flow.value = { ...current, stage: 'cancelled' }
        storage.removeItem(routes.storageKey)
        operation.value = 'idle'
        return true
      } catch (cause) {
        if (
          owner !== generation ||
          (!active.timedOut() && active.controller.signal.aborted)
        ) {
          return false
        }
        retainError(
          `${transportMessage(cause, routes.noun)} Cancellation was not confirmed; you can retry it.`,
          current,
        )
        operation.value = 'idle'
        schedulePoll(owner)
        return false
      } finally {
        active.finish()
      }
    },

    resume: (scopes) => {
      if (flow.value !== null || storage.getItem(routes.storageKey) === null) {
        return Promise.resolve(false)
      }
      generation += 1
      return fetchCurrent(generation, 'resuming', scopes)
    },

    reset: () => {
      generation += 1
      clearPoll()
      request?.abort()
      request = null
      flow.value = null
      routeScope = null
      operation.value = 'idle'
      clearRetainedError()
      error.value = null
      storage.removeItem(routes.storageKey)
    },
  }
}

function requireScope(scope: string | null): string {
  if (scope === null) {
    throw new Error('account-scoped QR flow has no account')
  }
  return scope
}

export function createMatrixOAuthQrStore(
  api: ApiClient,
  accounts: AccountsStore,
  options: MatrixOAuthQrStoreOptions = {},
): MatrixOAuthQrStore {
  const core = createMatrixOAuthQrFlowCore<MatrixOAuthQrFlow>(
    {
      storageKey: ACQUIRE_ACTIVE_FLOW_KEY,
      noun: 'QR sign-in',
      scopeOf: () => null,
      get: (_scope, flowId, requestSignal) =>
        api.GET('/v1/accounts/login/qr/{flow_id}', {
          params: { path: { flow_id: flowId } },
          signal: requestSignal,
        }),
      scan: (_scope, flowId, qrCodeData, requestSignal) =>
        api.POST('/v1/accounts/login/qr/{flow_id}/scan', {
          params: { path: { flow_id: flowId } },
          body: { qr_code_data: qrCodeData },
          signal: requestSignal,
        }),
      checkCode: (_scope, flowId, checkCode, requestSignal) =>
        api.POST('/v1/accounts/login/qr/{flow_id}/check-code', {
          params: { path: { flow_id: flowId } },
          body: { check_code: checkCode },
          signal: requestSignal,
        }),
      cancel: (_scope, flowId, requestSignal) =>
        api.DELETE('/v1/accounts/login/qr/{flow_id}', {
          params: { path: { flow_id: flowId } },
          signal: requestSignal,
        }),
      failureFallback: 'QR sign-in failed. Start a new flow and try again.',
      knownFailureMessage: (code) =>
        knownFailureMessage(ACQUIRE_FAILURE_MESSAGES, code),
      onDone: async () => accounts.refresh(),
    },
    options,
  )
  return {
    ...core,
    start: (userId, presentation) =>
      core.start(null, (requestSignal) =>
        api.POST('/v1/accounts/login/qr', {
          body: { expected_user_id: userId, presentation },
          signal: requestSignal,
        }),
      ),
    resume: () => core.resume([null]),
  }
}

export function createMatrixOAuthQrGrantStore(
  api: ApiClient,
  options: MatrixOAuthQrStoreOptions = {},
): MatrixOAuthQrGrantStore {
  const core = createMatrixOAuthQrFlowCore<MatrixOAuthQrGrantFlow>(
    {
      storageKey: GRANT_ACTIVE_FLOW_KEY,
      noun: 'device authorization',
      scopeOf: (flow) => flow.account_id,
      get: (scope, flowId, requestSignal) =>
        api.GET('/v1/accounts/{account_id}/login-grants/qr/{flow_id}', {
          params: {
            path: { account_id: requireScope(scope), flow_id: flowId },
          },
          signal: requestSignal,
        }),
      scan: (scope, flowId, qrCodeData, requestSignal) =>
        api.POST('/v1/accounts/{account_id}/login-grants/qr/{flow_id}/scan', {
          params: {
            path: { account_id: requireScope(scope), flow_id: flowId },
          },
          body: { qr_code_data: qrCodeData },
          signal: requestSignal,
        }),
      checkCode: (scope, flowId, checkCode, requestSignal) =>
        api.POST(
          '/v1/accounts/{account_id}/login-grants/qr/{flow_id}/check-code',
          {
            params: {
              path: { account_id: requireScope(scope), flow_id: flowId },
            },
            body: { check_code: checkCode },
            signal: requestSignal,
          },
        ),
      cancel: (scope, flowId, requestSignal) =>
        api.DELETE('/v1/accounts/{account_id}/login-grants/qr/{flow_id}', {
          params: {
            path: { account_id: requireScope(scope), flow_id: flowId },
          },
          signal: requestSignal,
        }),
      failureFallback:
        'Device authorization failed. Start a new flow and try again.',
      knownFailureMessage: (code) =>
        knownFailureMessage(GRANT_FAILURE_MESSAGES, code),
    },
    options,
  )
  return {
    ...core,
    start: (accountId, presentation) =>
      core.start(accountId, (requestSignal) =>
        api.POST('/v1/accounts/{account_id}/login-grants/qr', {
          params: { path: { account_id: accountId } },
          body: { presentation },
          signal: requestSignal,
        }),
      ),
    resume: (accountIds) => core.resume(accountIds),
  }
}
