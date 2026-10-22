/**
 * Register and delete one device's remote-push row (ADR 0113, issue 605).
 *
 * The device token is read from the OS on every PUT and is never written to
 * storage. The access token is never written either: a sign-out retry keeps
 * the peeked bearer in memory only, and a delete that did not finish keeps
 * the session so the next launch can still present it.
 */

export const PUSH_REGISTRATION_KEY = 'axon.push.registration'

export const DELETE_ATTEMPTS = 3

export const PUSH_DISCLOSURES = [
  'activity',
  'sender_and_room',
  'preview',
] as const

export type PushDisclosure = (typeof PUSH_DISCLOSURES)[number]

export type PushProvider = 'apns' | 'fcm'

export type PushEnvironment = 'sandbox' | 'production'

export type RefreshOutcome = 'refreshed' | 'unsupported' | 'failed'

export interface PutPushBody {
  provider: PushProvider
  token: string
  disclosure: PushDisclosure
  environment?: PushEnvironment
}

export interface StoredPushRegistration {
  id: string
  pendingDelete?: boolean
  /**
   * The delete was started by sign-out. The owning session is still the only
   * caller that can remove the row, so this stays until that delete succeeds
   * and the session is then dropped.
   */
  signOut?: boolean
}

export interface PushRegistrationDeps {
  remote: {
    provider: PushProvider
    deviceToken(): Promise<{
      token: string
      environment: PushEnvironment | null
    }>
  } | null
  signedIn: () => boolean
  enabled: () => boolean
  permissionGranted: () => boolean
  disclosure: () => PushDisclosure
  refreshSession: () => Promise<RefreshOutcome>
  onAccessTokenRefreshed: (listener: () => void) => () => void
  accessToken: () => string | null | Promise<string | null>
  onBeforeSignOut: (
    hook: (token: string | null) => void | Promise<void>,
  ) => () => void
  /**
   * Drop the local session without running sign-out hooks again. Used when a
   * sign-out delete succeeds after the original attempt, once the owning
   * bearer is no longer sitting in that attempt.
   */
  finishSignOut: () => void
  put: (accessToken: string, body: PutPushBody) => Promise<{ id: string }>
  remove: (accessToken: string, id: string) => Promise<void>
  storage: Storage
  scheduleRetry: (run: () => void) => void
  onRemoteFailure: (message: string | null) => void
}

const REFRESH_FAILED =
  'Axon could not refresh the sign-in before registering this device.'
const REGISTER_FAILED = 'Axon could not register this device for push.'
const TOKEN_UNUSABLE = 'This device did not provide a usable push token.'
const ENVIRONMENT_MISSING =
  'This build did not report a sandbox or production push environment.'
const DELETE_FAILED = 'Axon could not remove this device registration.'

/**
 * `push_session_refresh_required` is a 400 that writes no row.
 * The registration module does not import the API client; it recognizes the
 * code on the thrown value.
 */
export function isPushSessionRefreshRequired(error: unknown): boolean {
  if (typeof error !== 'object' || error === null) {
    return false
  }
  const record = error as { code?: unknown; error?: { code?: unknown } }
  if (record.code === 'push_session_refresh_required') {
    return true
  }
  return record.error?.code === 'push_session_refresh_required'
}

export function createPushRegistration(deps: PushRegistrationDeps): {
  reconcile: () => Promise<void>
  dispose: () => void
} {
  let generation = 0
  let retiring = false
  let chain: Promise<void> = Promise.resolve()

  const enqueue = (task: () => Promise<void>): Promise<void> => {
    const run = chain.then(task, task)
    chain = run.then(
      () => undefined,
      () => undefined,
    )
    return run
  }

  const stopRefresh = deps.onAccessTokenRefreshed(() => {
    // Check before bumping generation. A refresh that arrives after a
    // disable has been queued, but before that task runs, must not cancel
    // the DELETE. The listener's own register would then no-op.
    if (!wanted()) {
      return
    }
    const current = ++generation
    void enqueue(() => register(current))
  })

  const stopSignOut = deps.onBeforeSignOut((token) => {
    // The in-flight PUT has to finish before this DELETE, and this promise
    // has to finish before the session is dropped. A reload in between
    // leaves the row owned by a session this device can no longer present.
    retiring = true
    const current = ++generation
    return enqueue(() => signOutRegistration(current, token))
  })

  function wanted(): boolean {
    return (
      !retiring &&
      deps.remote !== null &&
      deps.signedIn() &&
      deps.enabled() &&
      deps.permissionGranted()
    )
  }

  async function register(current: number): Promise<void> {
    if (!wanted() || deps.remote === null) {
      return
    }
    const posted = await postRegistration(current)
    if (posted !== 'refresh' || current !== generation) {
      return
    }
    // The 400 wrote nothing. One more refresh, then one more PUT. A second
    // copy of the same code stops below.
    const outcome = await refreshOutcome()
    if (current !== generation) {
      return
    }
    if (outcome === 'failed') {
      deps.onRemoteFailure(REFRESH_FAILED)
      return
    }
    const again = await postRegistration(current)
    if (again === 'refresh' && current === generation) {
      deps.onRemoteFailure(REGISTER_FAILED)
    }
  }

  async function refreshOutcome(): Promise<RefreshOutcome> {
    try {
      return await deps.refreshSession()
    } catch {
      return 'failed'
    }
  }

  async function postRegistration(
    current: number,
  ): Promise<'ok' | 'refresh' | 'stop'> {
    if (!wanted() || deps.remote === null || current !== generation) {
      return 'stop'
    }
    const accessToken = await bearer()
    if (current !== generation) {
      return 'stop'
    }
    if (accessToken === null) {
      deps.onRemoteFailure(REGISTER_FAILED)
      return 'stop'
    }
    let device: { token: string; environment: PushEnvironment | null }
    try {
      device = await deps.remote.deviceToken()
    } catch {
      if (current === generation) {
        deps.onRemoteFailure(REGISTER_FAILED)
      }
      return 'stop'
    }
    if (current !== generation) {
      return 'stop'
    }
    if (!usableDeviceToken(device.token)) {
      deps.onRemoteFailure(TOKEN_UNUSABLE)
      return 'stop'
    }
    const environment = pushEnvironment(device.environment)
    if (deps.remote.provider === 'apns' && environment === null) {
      deps.onRemoteFailure(ENVIRONMENT_MISSING)
      return 'stop'
    }
    const body: PutPushBody = {
      provider: deps.remote.provider,
      token: device.token,
      disclosure: deps.disclosure(),
    }
    if (deps.remote.provider === 'apns' && environment !== null) {
      body.environment = environment
    }
    try {
      const result = await deps.put(accessToken, body)
      if (current !== generation) {
        if (!wanted()) {
          await deps.remove(accessToken, result.id).catch(() => undefined)
        }
        return 'stop'
      }
      writeStored(deps.storage, { id: result.id })
      deps.onRemoteFailure(null)
      return 'ok'
    } catch (error) {
      if (current === generation && isPushSessionRefreshRequired(error)) {
        return 'refresh'
      }
      if (current === generation) {
        deps.onRemoteFailure(REGISTER_FAILED)
      }
      return 'stop'
    }
  }

  async function signOutRegistration(
    current: number,
    token: string | null,
  ): Promise<void> {
    const stored = readStored(deps.storage)
    if (stored === null) {
      return
    }
    writeStored(deps.storage, {
      id: stored.id,
      pendingDelete: true,
      signOut: true,
    })
    await removeRegistration({
      generation: current,
      id: stored.id,
      token,
      attemptsLeft: DELETE_ATTEMPTS,
      capturedToken: token,
      signOut: true,
      completeSignOut: false,
    })
    const left = readStored(deps.storage)
    if (left?.id === stored.id && left.pendingDelete === true) {
      throw new Error('the push registration is still present')
    }
  }

  async function removeRegistration(options: {
    generation: number
    id: string
    token: string | null
    attemptsLeft: number
    capturedToken: string | null
    signOut: boolean
    completeSignOut: boolean
  }): Promise<void> {
    const token = options.token
    if (token === null || token === '') {
      failDelete(options)
      return
    }
    try {
      await deps.remove(token, options.id)
    } catch {
      failDelete(options)
      return
    }
    const stored = readStored(deps.storage)
    const finish = options.completeSignOut && stored?.signOut === true
    if (stored?.id === options.id) {
      deps.storage.removeItem(PUSH_REGISTRATION_KEY)
    }
    if (finish) {
      retiring = false
      deps.finishSignOut()
    }
    if (options.generation === generation) {
      deps.onRemoteFailure(null)
    }
  }

  function failDelete(options: {
    generation: number
    id: string
    attemptsLeft: number
    capturedToken: string | null
    signOut: boolean
  }): void {
    const signOut =
      options.signOut || readStored(deps.storage)?.signOut === true
    writeStored(
      deps.storage,
      signOut
        ? { id: options.id, pendingDelete: true, signOut: true }
        : { id: options.id, pendingDelete: true },
    )
    if (options.attemptsLeft <= 1) {
      if (options.generation === generation) {
        deps.onRemoteFailure(DELETE_FAILED)
      }
      return
    }
    const captured = options.capturedToken
    deps.scheduleRetry(() => {
      void enqueue(async () => {
        const stored = readStored(deps.storage)
        if (stored?.id !== options.id || stored.pendingDelete !== true) {
          return
        }
        // The captured bearer is the session that owns the row. A later
        // sign-in cannot delete it, so do not drop the retry when one starts.
        const token = captured ?? (await bearer())
        if (token === null) {
          return
        }
        await removeRegistration({
          generation: options.generation,
          id: options.id,
          token,
          attemptsLeft: options.attemptsLeft - 1,
          capturedToken: captured,
          signOut: stored.signOut === true,
          completeSignOut: stored.signOut === true,
        })
      })
    })
  }

  async function bearer(): Promise<string | null> {
    const token = await deps.accessToken()
    return typeof token === 'string' && token !== '' ? token : null
  }

  function reconcile(): Promise<void> {
    const current = ++generation
    return enqueue(async () => {
      if (deps.remote === null) {
        return
      }
      const pending = readStored(deps.storage)
      if (pending?.pendingDelete === true) {
        // Signed out, the owning bearer is gone and a new session's DELETE
        // would be a no-op. Leave the marker until that session is back.
        if (!deps.signedIn()) {
          return
        }
        const token = await bearer()
        if (current !== generation) {
          return
        }
        await removeRegistration({
          generation: current,
          id: pending.id,
          token,
          attemptsLeft: DELETE_ATTEMPTS,
          capturedToken: null,
          signOut: pending.signOut === true,
          completeSignOut: pending.signOut === true,
        })
        if (readStored(deps.storage)?.pendingDelete === true) {
          return
        }
        if (pending.signOut === true) {
          return
        }
      }
      if (!deps.signedIn()) {
        return
      }
      if (!deps.enabled() || !deps.permissionGranted()) {
        const stored = readStored(deps.storage)
        if (stored === null) {
          if (current === generation) {
            deps.onRemoteFailure(null)
          }
          return
        }
        const token = await bearer()
        if (current !== generation) {
          return
        }
        await removeRegistration({
          generation: current,
          id: stored.id,
          token,
          attemptsLeft: DELETE_ATTEMPTS,
          capturedToken: null,
          signOut: false,
          completeSignOut: false,
        })
        return
      }
      await register(current)
    })
  }

  return {
    reconcile,
    dispose() {
      generation += 1
      stopRefresh()
      stopSignOut()
    },
  }
}

function usableDeviceToken(token: string): boolean {
  if (token === '' || token !== token.trim() || token.length > 4096) {
    return false
  }
  for (let index = 0; index < token.length; index += 1) {
    const code = token.charCodeAt(index)
    if (code <= 0x1f || code === 0x7f) {
      return false
    }
  }
  return true
}

function pushEnvironment(value: string | null): PushEnvironment | null {
  return value === 'sandbox' || value === 'production' ? value : null
}

export function readStored(storage: Storage): StoredPushRegistration | null {
  let raw: string | null
  try {
    raw = storage.getItem(PUSH_REGISTRATION_KEY)
  } catch {
    return null
  }
  if (raw === null) {
    return null
  }
  try {
    const parsed = JSON.parse(raw) as unknown
    if (
      typeof parsed !== 'object' ||
      parsed === null ||
      Array.isArray(parsed)
    ) {
      return null
    }
    const id = (parsed as { id?: unknown }).id
    if (typeof id !== 'string' || id === '') {
      return null
    }
    const pending = (parsed as { pendingDelete?: unknown }).pendingDelete
    const signOut = (parsed as { signOut?: unknown }).signOut === true
    if (pending === true) {
      return signOut
        ? { id, pendingDelete: true, signOut: true }
        : { id, pendingDelete: true }
    }
    return { id }
  } catch {
    return null
  }
}

function writeStored(storage: Storage, value: StoredPushRegistration): void {
  try {
    storage.setItem(PUSH_REGISTRATION_KEY, JSON.stringify(value))
  } catch {
    // A full store must not throw out of the registration attempt.
  }
}
