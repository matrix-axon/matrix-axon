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
  /**
   * Resolves true when the sign-in is over and added no account. False when
   * it could not be cancelled, including when it had already finished.
   */
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
  // An error about the flow as it stands, which the next unchanged poll must
  // not wipe: it lasts until the flow moves to another stage.
  let retainedErrorAt: Pick<
    MatrixOAuthBrowserFlow,
    'flow_id' | 'stage'
  > | null = null

  function clearRetainedError(): void {
    retainedErrorAt = null
  }

  function retainError(message: string): void {
    const current = flow.value
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
    const retainCurrentError =
      retainedErrorAt?.flow_id === next.flow_id &&
      retainedErrorAt.stage === next.stage
    flow.value = next
    if (next.stage === 'failed') {
      clearRetainedError()
      error.value =
        (next.error_code === null || next.error_code === undefined
          ? undefined
          : FAILURE_MESSAGES[next.error_code]) ?? FAILURE_FALLBACK
    } else if (!retainCurrentError) {
      clearRetainedError()
      error.value = null
    }
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

  /**
   * Axon no longer knows the flow. It forgets one two minutes after it ends,
   * and this method sends the user away to another tab, app or device, so a
   * page that was frozen in the meantime can come back to a sign-in that
   * finished without it. The account list is what tells the two apart.
   */
  async function settleMissing(owner: number): Promise<void> {
    const known = flow.value
    storage.removeItem(ACTIVE_FLOW_KEY)
    operation.value = 'refreshing_accounts'
    await accounts.refresh()
    if (owner !== generation) {
      return
    }
    clearRetainedError()
    const account =
      known === null
        ? undefined
        : accounts.accounts.value.find(
            (candidate) =>
              candidate.user_id === known.expected_user_id &&
              candidate.state === 'active',
          )
    if (known !== null && account !== undefined) {
      // Axon refuses to start a sign-in for an account that is already
      // active, so an active one now is this sign-in's.
      completed.add(known.flow_id)
      flow.value = {
        flow_id: known.flow_id,
        expected_user_id: known.expected_user_id,
        stage: 'done',
        account_id: account.account_id,
      }
      error.value = null
    } else {
      flow.value = null
      error.value =
        known === null
          ? 'An earlier browser sign-in is no longer open. If its account is listed above, it finished; otherwise start again.'
          : 'This browser sign-in is no longer open. Start again to continue.'
    }
    operation.value = 'idle'
  }

  /** One read of the flow: applied, gone from Axon, failed, or superseded. */
  async function fetchOnce(
    owner: number,
    kind: 'polling' | 'resuming' | 'cancelling',
  ): Promise<'applied' | 'missing' | 'failed' | 'superseded'> {
    if (owner !== generation) {
      return 'superseded'
    }
    const flowId = flow.value?.flow_id ?? storage.getItem(ACTIVE_FLOW_KEY)
    if (flowId === null || flowId === '') {
      operation.value = 'idle'
      return 'superseded'
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
        return 'superseded'
      }
      if (result.error === undefined && result.data !== undefined) {
        await applyFlow(result.data.data, owner)
        return 'applied'
      }
      if (result.response.status === 404) {
        return 'missing'
      }
      clearRetainedError()
      error.value = apiErrorMessage(result.error)
      operation.value = 'idle'
      schedulePoll(owner, transportBackoffMs)
      return 'failed'
    } catch (cause) {
      if (
        owner !== generation ||
        (!active.timedOut() && active.controller.signal.aborted)
      ) {
        return 'superseded'
      }
      clearRetainedError()
      error.value = transportMessage(cause, 'Still checking the sign-in.')
      operation.value = 'idle'
      schedulePoll(owner, transportBackoffMs)
      return 'failed'
    } finally {
      active.finish()
    }
  }

  async function fetchCurrent(
    owner: number,
    kind: 'polling' | 'resuming',
  ): Promise<boolean> {
    const outcome = await fetchOnce(owner, kind)
    if (outcome === 'missing') {
      await settleMissing(owner)
    }
    return outcome === 'applied'
  }

  return {
    flow: computed(() => flow.value),
    operation: computed(() => operation.value),
    error,

    start: async (userId) => {
      const previous = flow.value
      // The flow this start replaces: the one on screen, or one a reload left
      // behind that could not be read back. Forgetting its id without
      // cancelling it would leave Axon holding the user's one sign-in slot
      // with nothing here able to release it.
      const abandoned =
        previous === null
          ? storage.getItem(ACTIVE_FLOW_KEY)
          : isTerminalMatrixOAuthBrowserFlow(previous)
            ? null
            : previous.flow_id
      generation += 1
      const owner = generation
      clearPoll()
      request?.abort()
      flow.value = null
      clearRetainedError()
      error.value = null
      operation.value = 'starting'
      storage.removeItem(ACTIVE_FLOW_KEY)
      if (abandoned !== null && abandoned !== '') {
        // Awaited, so the new request does not race the cancellation it
        // depends on; bounded and best-effort, so a failure only means the
        // create below reports the conflict.
        const controller = new AbortController()
        const timer = setTimeout(() => controller.abort(), requestTimeoutMs)
        await api
          .DELETE('/v1/accounts/login/browser/{flow_id}', {
            params: { path: { flow_id: abandoned } },
            signal: controller.signal,
          })
          .catch(() => {})
          .finally(() => clearTimeout(timer))
        if (owner !== generation) {
          return false
        }
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
      clearRetainedError()
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
          retainError(apiErrorMessage(result.error))
          operation.value = 'idle'
          schedulePoll(owner)
          return false
        }
      } catch (cause) {
        if (
          owner !== generation ||
          (!active.timedOut() && active.controller.signal.aborted)
        ) {
          return false
        }
        retainError(
          transportMessage(
            cause,
            'Cancellation was not confirmed; you can retry it.',
          ),
        )
        operation.value = 'idle'
        schedulePoll(owner)
        return false
      } finally {
        active.finish()
      }
      // Axon accepts a cancellation it can no longer honour: once the
      // sign-in is approved and being finished, the request succeeds and
      // changes nothing. Only the flow itself says how it ended.
      const outcome = await fetchOnce(owner, 'cancelling')
      if (outcome === 'superseded') {
        return false
      }
      if (outcome === 'missing') {
        // Nothing left to cancel or to finish.
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
      }
      if (outcome === 'failed') {
        retainError(
          'Cancellation was sent, but Axon has not confirmed it. Still checking the sign-in.',
        )
        return false
      }
      const settled = flow.value
      if (settled === null) {
        return true
      }
      if (!isTerminalMatrixOAuthBrowserFlow(settled)) {
        retainError(
          settled.stage === 'finalizing'
            ? 'Too late to cancel: the sign-in was already approved and is finishing.'
            : 'Cancellation was not confirmed; you can retry it.',
        )
        return false
      }
      return settled.stage !== 'done'
    },

    reset: () => {
      generation += 1
      clearPoll()
      request?.abort()
      request = null
      flow.value = null
      operation.value = 'idle'
      clearRetainedError()
      error.value = null
      storage.removeItem(ACTIVE_FLOW_KEY)
    },
  }
}
