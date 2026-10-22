import { addPluginListener, invoke } from '@tauri-apps/api/core'
import type { MessageNotification, NotificationClick } from './notifications'
import { notificationClickFromWake } from './notifications'
import { notificationCollapseId } from '../push/collapse'

export type RemotePushProvider = 'apns' | 'fcm'

export interface RemotePush {
  provider: RemotePushProvider
  deviceToken(): Promise<{
    token: string
    environment: 'sandbox' | 'production' | null
  }>
  deliveredEventIds(): Promise<readonly string[]>
  onWake(handler: (click: NotificationClick) => void): () => void
  postCollapsed(
    message: MessageNotification,
    collapseId?: string,
  ): Promise<void>
}

/** iOS and iPadOS are `ios`. Android is `android` or the older `androideabi`. */
export function remotePushFor(shellPlatform: string): RemotePush | null {
  if (shellPlatform === 'ios') {
    return createRemotePush('apns')
  }
  if (shellPlatform === 'android' || shellPlatform === 'androideabi') {
    return createRemotePush('fcm')
  }
  return null
}

const SEEN_WAKES_KEY = 'axon.push.seen-wakes'
const SEEN_WAKES_LIMIT = 50

export function loadSeenWakes(storage: Storage): string[] {
  try {
    const raw = storage.getItem(SEEN_WAKES_KEY)
    if (raw === null) {
      return []
    }
    const parsed = JSON.parse(raw) as unknown
    if (!Array.isArray(parsed)) {
      return []
    }
    return parsed.filter(
      (id): id is string => typeof id === 'string' && id !== '',
    )
  } catch {
    return []
  }
}

export function rememberSeenWake(
  storage: Storage,
  seen: string[],
  eventId: string,
): void {
  if (seen.includes(eventId)) {
    return
  }
  seen.push(eventId)
  if (seen.length > SEEN_WAKES_LIMIT) {
    seen.shift()
  }
  try {
    storage.setItem(SEEN_WAKES_KEY, JSON.stringify(seen))
  } catch {
    // A full store must not drop the navigation.
  }
}

export function clearSeenWakes(storage: Storage, seen: string[]): void {
  seen.length = 0
  try {
    storage.removeItem(SEEN_WAKES_KEY)
  } catch {
    // The in-memory list is already empty. A later tap still navigates.
  }
}

export interface SeenWakeGate {
  seen: string[]
  /**
   * Set when a retained wake just opened. The next live delivery of this
   * id is the native copy of that same wake, not a new tap.
   */
  suppressLive: string | null
}

/**
 * Whether this delivery should navigate.
 *
 * A live delivery is a tap. It navigates, except for the one twin of a
 * retained wake this document just opened. It records the id so the next
 * document can ignore the copy the native side still holds across a reload.
 *
 * A retained delivery is the startup `pending_wake` read. It navigates only
 * when this document has not already recorded the id. That read consumed
 * the native slot, so the record is dropped and a later tap of the same
 * event navigates.
 */
export function acceptWake(
  storage: Storage,
  gate: SeenWakeGate,
  eventId: string,
  source: 'live' | 'retained',
): boolean {
  if (source === 'live') {
    if (gate.suppressLive === eventId) {
      gate.suppressLive = null
      return false
    }
    rememberSeenWake(storage, gate.seen, eventId)
    return true
  }
  const replay = gate.seen.includes(eventId)
  clearSeenWakes(storage, gate.seen)
  gate.suppressLive = replay ? null : eventId
  return !replay
}

/** The queued twin of a retained wake did not arrive. A later tap may open. */
export function releaseRetainedTwin(gate: SeenWakeGate): void {
  gate.suppressLive = null
}

function createRemotePush(provider: RemotePushProvider): RemotePush {
  return {
    provider,
    async deviceToken() {
      const result = await invoke<{ token?: unknown; environment?: unknown }>(
        'plugin:remote-push|device_token',
      )
      if (typeof result?.token !== 'string' || result.token === '') {
        throw new Error('the device did not provide a push token')
      }
      const environment = result.environment
      return {
        token: result.token,
        environment:
          environment === 'sandbox' || environment === 'production'
            ? environment
            : null,
      }
    },
    async deliveredEventIds() {
      const result = await invoke<{ eventIds?: unknown }>(
        'plugin:remote-push|delivered_event_ids',
      )
      if (!Array.isArray(result?.eventIds)) {
        return []
      }
      return result.eventIds.filter(
        (id): id is string => typeof id === 'string' && id !== '',
      )
    },
    onWake(handler) {
      if (!inTauri()) {
        return () => {}
      }
      const storage = wakeStorage()
      const gate: SeenWakeGate = {
        seen: loadSeenWakes(storage),
        suppressLive: null,
      }
      let stopped = false
      let sawLive = false
      let unregister: (() => void) | null = null
      let releaseTimer: ReturnType<typeof setTimeout> | null = null
      const take = (payload: unknown, source: 'live' | 'retained') => {
        if (stopped) {
          return
        }
        const click = notificationClickFromWake(payload)
        if (click?.eventId == null || click.eventId === '') {
          // An empty retained read means the native slot was clear. Drop ids
          // left by an earlier document. A live tap during this read has
          // already filled the slot again, so those ids stay.
          if (source === 'retained' && !sawLive) {
            clearSeenWakes(storage, gate.seen)
            gate.suppressLive = null
          }
          return
        }
        if (source === 'live') {
          sawLive = true
        }
        if (!acceptWake(storage, gate, click.eventId, source)) {
          return
        }
        handler(click)
      }
      void (async () => {
        try {
          const listener = await addPluginListener<unknown>(
            'remote-push',
            'wake',
            (payload) => {
              take(payload, 'live')
            },
          )
          unregister = () => {
            void listener.unregister()
          }
          if (stopped) {
            unregister()
            return
          }
          take(await invoke('plugin:remote-push|pending_wake'), 'retained')
          // A live twin of that retained wake may already be queued. Release
          // the one-shot suppress on the next turn, after that twin.
          releaseTimer = setTimeout(() => {
            releaseTimer = null
            releaseRetainedTwin(gate)
          }, 0)
        } catch {
          // A missing command is a desktop binary. There is no token to log.
        }
      })()
      return () => {
        stopped = true
        if (releaseTimer !== null) {
          clearTimeout(releaseTimer)
          releaseTimer = null
        }
        unregister?.()
      }
    },
    async postCollapsed(message, collapseId) {
      const id = collapseId ?? (await notificationCollapseId(message.roomId))
      if (id === null) {
        throw new Error('could not build a notification collapse id')
      }
      await invoke('plugin:remote-push|post_local', {
        args: {
          title: message.title,
          body: message.body,
          collapseId: id,
          accountId: message.accountId,
          roomId: message.roomId,
          eventId: message.eventId,
          threadRootId: message.threadRootId,
        },
      })
    },
  }
}

function wakeStorage(): Storage {
  try {
    return sessionStorage
  } catch {
    return emptyStorage()
  }
}

function emptyStorage(): Storage {
  const values = new Map<string, string>()
  return {
    get length() {
      return values.size
    },
    clear() {
      values.clear()
    },
    getItem(key: string) {
      return values.get(key) ?? null
    },
    key(index: number) {
      return [...values.keys()][index] ?? null
    },
    removeItem(key: string) {
      values.delete(key)
    },
    setItem(key: string, value: string) {
      values.set(key, value)
    },
  }
}

function inTauri(): boolean {
  return (
    typeof window !== 'undefined' &&
    '__TAURI_INTERNALS__' in (window as unknown as Record<string, unknown>)
  )
}
