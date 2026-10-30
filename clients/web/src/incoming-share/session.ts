import { effect } from '@preact/signals'
import type { CompositeAuthProvider } from '../auth/composite'
import type { SecureStorage } from '../platform'
import type { IncomingSharePort } from './types'
import type { IncomingShareStore } from './store'

/** Access-token publication is ordered with native writes, but refresh-token
 * rotation stays exclusively in the existing main-app OAuth provider. */
export function connectIncomingShareSession(
  port: IncomingSharePort,
  store: IncomingShareStore,
  auth: CompositeAuthProvider,
  storage: SecureStorage,
  server: string,
) {
  let previous = auth.sessionId.peek()
  let generation = 0
  let tail = Promise.resolve()
  store.setSessionReady(() => tail)
  const publish = () => {
    const identity = auth.sessionId.value
    const token = auth.peekToken()
    const signedIn = auth.signedIn.value
    const replace = previous !== identity
    previous = identity
    const current = ++generation
    if (replace) store.reset()
    // Snapshot persistent storage synchronously with the active auth state.
    // A one-time session can never borrow a different persisted credential.
    let persisted: string | null = null
    let expiresAt: number | undefined
    const raw = storage.getItem('axon.oauth.session')
    if (raw !== null && auth.oauth.signedIn.peek()) {
      try {
        const value = JSON.parse(raw) as {
          accessToken?: string
          expiresAt?: number
        }
        if (value.accessToken === token) {
          persisted = token
          expiresAt = value.expiresAt
        }
      } catch {
        /* An invalid stored value grants no extension access. */
      }
    } else if (storage.getItem('axon.token') === token) persisted = token
    tail = tail
      .catch(() => {})
      .then(async () => {
        if (replace)
          await port.call('publish', { server, token: null, replace: true })
        await storage.settled()
        // Identity invalidations must not be skipped by a subsequent refresh.
        await port.call('publish', {
          server,
          token: signedIn ? persisted : null,
          expiresAt,
          replace: false,
        })
        if (current === generation) await store.refresh()
      })
    void tail.catch(() => {
      if (current === generation)
        store.error.value =
          'Sharing credentials are unavailable. Open Axon and refresh your session before resuming.'
    })
  }
  const stop = effect(publish)
  const unsubscribe = port.subscribe(() => {
    void tail.then(
      () => store.refresh(),
      () => {},
    )
  })
  const removeHook = auth.onBeforeSignOut(async () => {
    store.reset()
    // Fence native dispatch before finishing the main-app sign-out.
    await tail.catch(() => {})
    await port.call('publish', { server, token: null, replace: true })
  })
  return () => {
    stop()
    unsubscribe()
    removeHook()
    generation += 1
  }
}
