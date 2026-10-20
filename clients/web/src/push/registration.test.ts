import { describe, expect, it } from 'vitest'
import { memoryStorage } from '../test/memory-storage'
import {
  PUSH_REGISTRATION_KEY,
  createPushRegistration,
  isPushSessionRefreshRequired,
  type PushEnvironment,
  type PushProvider,
  type PushRegistrationDeps,
  type PutPushBody,
  type RefreshOutcome,
} from './registration'

const REG_ID = '6b53f7f0-0000-4000-8000-0000000000aa'

function refreshRequired(): Error & { code: string } {
  const error = new Error('the push registration was not accepted') as Error & {
    code: string
  }
  error.code = 'push_session_refresh_required'
  return error
}

async function drain(): Promise<void> {
  for (let step = 0; step < 8; step += 1) {
    await Promise.resolve()
  }
}

function harness(options?: {
  provider?: PushProvider
  token?: string
  environment?: PushEnvironment | null
  remote?: PushRegistrationDeps['remote']
}) {
  const storage = memoryStorage()
  const puts: Array<{ token: string; body: PutPushBody }> = []
  const removed: Array<{ token: string; id: string }> = []
  const retries: Array<() => void> = []
  const failures: Array<string | null> = []
  const refreshListeners: Array<() => void> = []
  let signOutHook: (token: string | null) => void | Promise<void> = () => {}
  let access = 'access-1'
  let signedIn = true
  let enabled = true
  let permissionGranted = true
  let disclosure: PutPushBody['disclosure'] = 'activity'
  let refreshOutcome: RefreshOutcome = 'refreshed'
  let refreshCount = 0
  let putFailures = 0
  let removeError: Error | null = null
  let finishedSignOuts = 0

  const remote =
    options && 'remote' in options
      ? options.remote!
      : {
          provider: options?.provider ?? 'apns',
          deviceToken: () =>
            Promise.resolve({
              token: options?.token ?? 'device-token',
              environment:
                options?.environment === undefined
                  ? 'sandbox'
                  : options.environment,
            }),
        }

  const deps: PushRegistrationDeps = {
    remote,
    signedIn: () => signedIn,
    enabled: () => enabled,
    permissionGranted: () => permissionGranted,
    disclosure: () => disclosure,
    refreshSession: () => {
      refreshCount += 1
      if (refreshOutcome === 'refreshed') {
        access = `access-${refreshCount + 1}`
      }
      return Promise.resolve(refreshOutcome)
    },
    onAccessTokenRefreshed: (listener) => {
      refreshListeners.push(listener)
      return () => {}
    },
    accessToken: () => access,
    onBeforeSignOut: (hook) => {
      signOutHook = hook
      return () => {}
    },
    finishSignOut: () => {
      finishedSignOuts += 1
      signedIn = false
    },
    put: (token, body) => {
      puts.push({ token, body })
      if (putFailures > 0) {
        putFailures -= 1
        return Promise.reject(refreshRequired())
      }
      return Promise.resolve({ id: REG_ID })
    },
    remove: (token, id) => {
      removed.push({ token, id })
      if (removeError !== null) {
        return Promise.reject(removeError)
      }
      return Promise.resolve()
    },
    storage,
    scheduleRetry: (run) => {
      retries.push(run)
    },
    onRemoteFailure: (message) => {
      failures.push(message)
    },
  }

  return {
    registration: createPushRegistration(deps),
    storage,
    puts,
    removed,
    retries,
    failures,
    refreshCount: () => refreshCount,
    setRefreshOutcome: (value: RefreshOutcome) => {
      refreshOutcome = value
    },
    setAccess: (value: string) => {
      access = value
    },
    setSignedIn: (value: boolean) => {
      signedIn = value
    },
    setEnabled: (value: boolean) => {
      enabled = value
    },
    setPermission: (value: boolean) => {
      permissionGranted = value
    },
    setDisclosure: (value: PutPushBody['disclosure']) => {
      disclosure = value
    },
    failPuts: (count: number) => {
      putFailures = count
    },
    failRemove: () => {
      removeError = new Error('offline')
    },
    succeedRemove: () => {
      removeError = null
    },
    emitRefresh: () => {
      for (const listener of refreshListeners) {
        listener()
      }
    },
    signOut: (token: string | null) => signOutHook(token),
    finishedSignOuts: () => finishedSignOuts,
    enabled: () => enabled,
  }
}

describe('isPushSessionRefreshRequired', () => {
  it('recognizes the registration error code', () => {
    expect(isPushSessionRefreshRequired(refreshRequired())).toBe(true)
    expect(
      isPushSessionRefreshRequired({
        error: { code: 'push_session_refresh_required', message: 'refresh' },
      }),
    ).toBe(true)
    expect(isPushSessionRefreshRequired(new Error('no'))).toBe(false)
  })
})

describe('createPushRegistration', () => {
  it('registers with the current access token and does not refresh first', async () => {
    const push = harness()
    await push.registration.reconcile()
    expect(push.refreshCount()).toBe(0)
    expect(push.puts).toEqual([
      {
        token: 'access-1',
        body: {
          provider: 'apns',
          token: 'device-token',
          disclosure: 'activity',
          environment: 'sandbox',
        },
      },
    ])
    expect(JSON.parse(push.storage.getItem(PUSH_REGISTRATION_KEY)!)).toEqual({
      id: REG_ID,
    })
    expect(push.failures.at(-1)).toBeNull()
  })

  it('registers a non-expiring credential without waiting for a new access token', async () => {
    const push = harness()
    push.setRefreshOutcome('unsupported')
    push.setAccess('pasted-token')
    await push.registration.reconcile()
    expect(push.refreshCount()).toBe(0)
    expect(push.puts).toEqual([
      {
        token: 'pasted-token',
        body: {
          provider: 'apns',
          token: 'device-token',
          disclosure: 'activity',
          environment: 'sandbox',
        },
      },
    ])
  })

  it('does not PUT again when the refresh the server asked for fails', async () => {
    const push = harness()
    push.failPuts(1)
    push.setRefreshOutcome('failed')
    await push.registration.reconcile()
    expect(push.puts).toHaveLength(1)
    expect(push.puts[0]?.token).toBe('access-1')
    expect(push.refreshCount()).toBe(1)
    expect(push.storage.getItem(PUSH_REGISTRATION_KEY)).toBeNull()
    expect(push.failures.at(-1)).toMatch(/refresh the sign-in/)
  })

  it('refreshes when the server asks, then PUTs with the newer token', async () => {
    const push = harness()
    push.failPuts(1)
    await push.registration.reconcile()
    expect(push.refreshCount()).toBe(1)
    expect(push.puts.map((put) => put.token)).toEqual(['access-1', 'access-2'])
    expect(JSON.parse(push.storage.getItem(PUSH_REGISTRATION_KEY)!)).toEqual({
      id: REG_ID,
    })
  })

  it('stops when the server asks for a refresh a second time', async () => {
    const push = harness()
    push.failPuts(2)
    await push.registration.reconcile()
    expect(push.refreshCount()).toBe(1)
    expect(push.puts).toHaveLength(2)
    expect(push.storage.getItem(PUSH_REGISTRATION_KEY)).toBeNull()
    expect(push.failures.at(-1)).toMatch(/could not register/)
  })

  it('omits environment for FCM', async () => {
    const push = harness({
      provider: 'fcm',
      token: 'fcm-token',
      environment: null,
    })
    await push.registration.reconcile()
    const body = push.puts[0]?.body
    expect(body?.provider).toBe('fcm')
    expect(body?.token).toBe('fcm-token')
    expect(body !== undefined && 'environment' in body).toBe(false)
  })

  it('does not PUT a token with surrounding whitespace', async () => {
    const push = harness({ token: ' device-token ' })
    await push.registration.reconcile()
    expect(push.puts).toEqual([])
    expect(push.failures.at(-1)).toMatch(/usable push token/)
  })

  it('does not PUT an APNs token with no environment', async () => {
    const push = harness({ environment: null })
    await push.registration.reconcile()
    expect(push.puts).toEqual([])
    expect(push.failures.at(-1)).toMatch(/sandbox or production/)
  })

  it('does nothing when this platform has no remote push', async () => {
    const push = harness({ remote: null })
    await push.registration.reconcile()
    expect(push.refreshCount()).toBe(0)
    expect(push.puts).toEqual([])
    expect(push.removed).toEqual([])
  })

  it('deletes the stored registration when notifications are turned off', async () => {
    const push = harness()
    await push.registration.reconcile()
    push.setEnabled(false)
    await push.registration.reconcile()
    expect(push.removed).toEqual([{ token: 'access-1', id: REG_ID }])
    expect(push.storage.getItem(PUSH_REGISTRATION_KEY)).toBeNull()
    expect(push.enabled()).toBe(false)
  })

  it('keeps the local opt-out when DELETE fails and does not store the bearer', async () => {
    const push = harness()
    await push.registration.reconcile()
    push.failRemove()
    push.setEnabled(false)
    await push.registration.reconcile()
    expect(push.enabled()).toBe(false)
    const stored = push.storage.getItem(PUSH_REGISTRATION_KEY)
    expect(JSON.parse(stored!)).toEqual({ id: REG_ID, pendingDelete: true })
    expect(stored).not.toContain('access-')
    expect(push.retries).toHaveLength(1)
  })

  it('deletes with the peeked sign-out token before the session can be dropped', async () => {
    const push = harness()
    await push.registration.reconcile()
    const done = push.signOut('peeked-token')
    await done
    expect(push.removed).toEqual([{ token: 'peeked-token', id: REG_ID }])
    expect(push.storage.getItem(PUSH_REGISTRATION_KEY)).toBeNull()
    expect(push.finishedSignOuts()).toBe(0)
  })

  it('keeps a failed sign-out delete and finishes it on the next launch', async () => {
    const push = harness()
    await push.registration.reconcile()
    push.failRemove()
    await expect(push.signOut('peeked-token')).rejects.toThrow(/still present/)
    const stored = push.storage.getItem(PUSH_REGISTRATION_KEY)!
    expect(stored).not.toContain('peeked-token')
    expect(JSON.parse(stored)).toEqual({
      id: REG_ID,
      pendingDelete: true,
      signOut: true,
    })
    expect(push.retries).toHaveLength(1)
    push.succeedRemove()
    await push.registration.reconcile()
    expect(push.removed.at(-1)).toEqual({ token: 'access-1', id: REG_ID })
    expect(push.storage.getItem(PUSH_REGISTRATION_KEY)).toBeNull()
    expect(push.finishedSignOuts()).toBe(1)
    expect(push.puts).toHaveLength(1)
  })

  it('still deletes when a token refresh arrives after notifications are turned off', async () => {
    const push = harness()
    await push.registration.reconcile()
    push.setEnabled(false)
    const pending = push.registration.reconcile()
    push.emitRefresh()
    await pending
    expect(push.removed).toEqual([{ token: 'access-1', id: REG_ID }])
    expect(push.storage.getItem(PUSH_REGISTRATION_KEY)).toBeNull()
    expect(push.puts).toHaveLength(1)
  })

  it('re-PUTs a disclosure change without refreshing first', async () => {
    const push = harness()
    await push.registration.reconcile()
    expect(push.refreshCount()).toBe(0)
    push.setDisclosure('preview')
    await push.registration.reconcile()
    expect(push.refreshCount()).toBe(0)
    expect(push.puts[1]?.body.disclosure).toBe('preview')
    expect(push.puts[1]?.body.environment).toBe('sandbox')
  })

  it('re-PUTs after a refresh it did not request, without calling refresh again', async () => {
    const push = harness()
    await push.registration.reconcile()
    const before = push.refreshCount()
    push.emitRefresh()
    await drain()
    expect(push.refreshCount()).toBe(before)
    expect(push.puts).toHaveLength(2)
  })
})
