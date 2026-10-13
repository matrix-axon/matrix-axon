import {
  computed,
  signal,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import { apiErrorMessage, causeField, type ApiClient } from '../api/client'
import type { components } from '../api/schema'
import type { AccountsStore } from './accounts'

export type MatrixOAuthBrowserFlow =
  components['schemas']['MatrixOAuthBrowserFlowDto']

export type MatrixOAuthBrowserOperation =
  | 'idle'
  | 'starting'
  | 'resuming'
  | 'polling'
  | 'refreshing_accounts'
  | 'cancelling'

export interface MatrixOAuthBrowserStore {
  flow: ReadonlySignal<MatrixOAuthBrowserFlow | null>
  operation: ReadonlySignal<MatrixOAuthBrowserOperation>
  /** Writable so the banner showing it can dismiss it. */
  error: Signal<string | null>
  start(userId: string): Promise<boolean>
  /** Pick a flow back up after a reload. Resolves false when there is none. */
  resume(): Promise<boolean>
  /** Whether a reload left a flow to resume, without touching the network. */
  hasStoredFlow(): boolean
  /** Ask now instead of waiting for the next poll. */
  checkNow(): void
  cancel(): Promise<boolean>
  reset(): void
}

interface MatrixOAuthBrowserStoreOptions {
  storage?: Storage
  requestTimeoutMs?: number
  pollDelayMs?: number
  transportBackoffMs?: number
}

const ACTIVE_FLOW_KEY = 'axon.matrix-oauth-browser.flow-id'
const REQUEST_TIMEOUT_MS = 15_000
// Axon asks the authorization server every five seconds at best, so a faster
// poll here would only re-read the same answer.
const POLL_DELAY_MS = 2_000
const TRANSPORT_BACKOFF_MS = 5_000

const TERMINAL_STAGES = new Set<MatrixOAuthBrowserFlow['stage']>([
  'done',
  'failed',
  'cancelled',
])

// The wording of docs/matrix-oauth-browser-sign-in.md, "Failure codes".
const FAILURE_MESSAGES: Record<string, string> = {
  unsupported:
    'This homeserver does not support browser sign-in. Try a password or QR sign-in.',
  authorization_denied:
    'Sign-in was declined in the browser. Start again if that was a mistake.',
  authorization_expired: 'The sign-in link expired. Start again.',
  user_mismatch:
    'You approved as a different account. Sign out in the browser, or correct the Matrix user ID, then start again.',
  conflict: 'This account was signed in another way. Nothing more is needed.',
  timeout: 'The homeserver is not responding. Try again shortly.',
  upstream:
    'Sign-in failed at the homeserver. Try again, and contact its operator if it keeps failing.',
  internal:
    'Sign-in failed on this Axon server. Try again or check the server logs.',
}
// The code set is open: an unknown one is a generic failure.
const FAILURE_FALLBACK = 'Browser sign-in failed. Start again.'

export function isTerminalMatrixOAuthBrowserFlow(
  flow: Pick<MatrixOAuthBrowserFlow, 'stage'>,
): boolean {
  return TERMINAL_STAGES.has(flow.stage)
}

function transportMessage(cause: unknown, afterTimeout: string): string {
  // `causeField` because a `DOMException` is not `instanceof Error` under
  // jsdom, and both names because WebKit raises `AbortError` where the spec
  // says `TimeoutError`; see the same function in `matrix-oauth-qr.ts`.
  const name = causeField(cause, 'name')
  if (name === 'AbortError' || name === 'TimeoutError') {
    return `The request to Axon timed out. ${afterTimeout}`
  }
  return cause instanceof Error
    ? `Could not reach Axon: ${cause.message}`
    : 'Could not reach Axon. Check the connection and try again.'
}

/**
 * One browser sign-in (ADR 0117): create, poll until terminal, cancel.
 *
 * The same discipline as the QR flows in `matrix-oauth-qr.ts`, without their
 * input steps: only the opaque flow id reaches session storage, the link and
 * code live in memory, every request is bounded, and a generation counter
 * keeps a late response from reviving a cancelled or replaced flow.
 */
export function createMatrixOAuthBrowserStore(
  api: ApiClient,
  accounts: AccountsStore,
  options: MatrixOAuthBrowserStoreOptions = {},
): MatrixOAuthBrowserStore {
  const storage = options.storage ?? window.sessionStorage
  const requestTimeoutMs = options.requestTimeoutMs ?? REQUEST_TIMEOUT_MS
  const pollDelayMs = options.pollDelayMs ?? POLL_DELAY_MS
  const transportBackoffMs = options.transportBackoffMs ?? TRANSPORT_BACKOFF_MS
  const flow = signal<MatrixOAuthBrowserFlow | null>(null)
  const operation = signal<MatrixOAuthBrowserOperation>('idle')
  const error = signal<string | null>(null)
  let generation = 0
  let request: AbortController | null = null
  let pollTimer: ReturnType<typeof setTimeout> | null = null
  const completed = new Set<string>()

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

  function pending(): boolean {
    return flow.value === null
      ? storage.getItem(ACTIVE_FLOW_KEY) !== null
      : !isTerminalMatrixOAuthBrowserFlow(flow.value)
  }

  function schedulePoll(owner: number, delay = pollDelayMs): void {
    clearPoll()
    if (owner !== generation || !pending()) {
      return
    }
    pollTimer = setTimeout(() => {
      pollTimer = null
      void fetchCurrent(owner, 'polling')
    }, delay)
  }

  async function applyFlow(
    next: MatrixOAuthBrowserFlow,
    owner: number,
  ): Promise<void> {
    if (owner !== generation) {
      return
    }
    const runCompletion = next.stage === 'done' && !completed.has(next.flow_id)
    if (runCompletion) {
      completed.add(next.flow_id)
      operation.value = 'refreshing_accounts'
    }
    flow.value = next
    error.value =
      next.stage === 'failed'
        ? ((next.error_code === null || next.error_code === undefined
            ? undefined
            : FAILURE_MESSAGES[next.error_code]) ?? FAILURE_FALLBACK)
        : null
    if (isTerminalMatrixOAuthBrowserFlow(next)) {
      clearPoll()
      storage.removeItem(ACTIVE_FLOW_KEY)
      if (runCompletion) {
        // The new account must be in the list before the panel offers to
        // verify it.
        await accounts.refresh()
        if (owner !== generation) {
          return
        }
      }
      operation.value = 'idle'
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
    clearPoll()
    operation.value = kind
    const active = beginRequest()
    try {
      const result = await api.GET('/v1/accounts/login/browser/{flow_id}', {
        params: { path: { flow_id: flowId } },
        signal: active.controller.signal,
      })
      if (owner !== generation || active.controller.signal.aborted) {
        return false
      }
      if (result.error === undefined && result.data !== undefined) {
        await applyFlow(result.data.data, owner)
        return true
      }
      if (result.response.status === 404) {
        // Axon forgets a flow some time after it ends, and at once on a
        // restart; either way there is nothing left to wait for.
        storage.removeItem(ACTIVE_FLOW_KEY)
        flow.value = null
        error.value =
          'This browser sign-in is no longer open. Start again to continue.'
        operation.value = 'idle'
        return false
      }
      error.value = apiErrorMessage(result.error)
      operation.value = 'idle'
      schedulePoll(owner, transportBackoffMs)
      return false
    } catch (cause) {
      if (
        owner !== generation ||
        (!active.timedOut() && active.controller.signal.aborted)
      ) {
        return false
      }
      error.value = transportMessage(cause, 'Still checking the sign-in.')
      operation.value = 'idle'
      schedulePoll(owner, transportBackoffMs)
      return false
    } finally {
      active.finish()
    }
  }

  return {
    flow: computed(() => flow.value),
    operation: computed(() => operation.value),
    error,

    start: async (userId) => {
      const previous = flow.value
      generation += 1
      const owner = generation
      clearPoll()
      request?.abort()
      flow.value = null
      error.value = null
      operation.value = 'starting'
      storage.removeItem(ACTIVE_FLOW_KEY)
      if (previous !== null && !isTerminalMatrixOAuthBrowserFlow(previous)) {
        const controller = new AbortController()
        const timer = setTimeout(() => controller.abort(), requestTimeoutMs)
        void api
          .DELETE('/v1/accounts/login/browser/{flow_id}', {
            params: { path: { flow_id: previous.flow_id } },
            signal: controller.signal,
          })
          .catch(() => {})
          .finally(() => clearTimeout(timer))
      }
      const active = beginRequest()
      try {
        const result = await api.POST('/v1/accounts/login/browser', {
          body: { expected_user_id: userId },
          signal: active.controller.signal,
        })
        if (owner !== generation || active.controller.signal.aborted) {
          return false
        }
        if (result.error !== undefined || result.data === undefined) {
          // The server's own sentence: a refusal here (a bad user ID, an
          // account already signed in, another sign-in open) is specific in a
          // way a message keyed on the error code could not be.
          error.value = apiErrorMessage(result.error)
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
        error.value = transportMessage(cause, 'Try again.')
        operation.value = 'idle'
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

    hasStoredFlow: () => storage.getItem(ACTIVE_FLOW_KEY) !== null,

    checkNow: () => {
      // Only in the gap between polls: a request already in flight is the
      // check, and replacing it would discard an answer that is on its way.
      if (operation.value === 'idle' && flow.value !== null && pending()) {
        void fetchCurrent(generation, 'polling')
      }
    },

    cancel: async () => {
      const current = flow.value
      if (current === null || isTerminalMatrixOAuthBrowserFlow(current)) {
        return true
      }
      const owner = generation
      clearPoll()
      operation.value = 'cancelling'
      error.value = null
      const active = beginRequest()
      try {
        const result = await api.DELETE(
          '/v1/accounts/login/browser/{flow_id}',
          {
            params: { path: { flow_id: current.flow_id } },
            signal: active.controller.signal,
          },
        )
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
        // The link and code go with the flow; nothing may show them again.
        flow.value = {
          flow_id: current.flow_id,
          expected_user_id: current.expected_user_id,
          stage: 'cancelled',
        }
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
        error.value = transportMessage(
          cause,
          'Cancellation was not confirmed; you can retry it.',
        )
        operation.value = 'idle'
        schedulePoll(owner)
        return false
      } finally {
        active.finish()
      }
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
