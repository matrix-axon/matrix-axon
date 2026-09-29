import { effect, signal, type Signal } from '@preact/signals'
import type { ApiClient } from '../api/client'
import { preferenceChange } from '../api/frames'
import type { LiveConnection } from './live-connection'

export const SPACE_ORDER_KEY = 'space_order'

/** A failed save waits this long, then twice as long, up to the cap. */
const SAVE_RETRY_BASE_MS = 1000
const SAVE_RETRY_MAX_MS = 30_000

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i

export interface SpaceOrderStore {
  /** Local moves before this resolves stay in `settings.spaceOrder` only. */
  hydrate(): Promise<void>
  move(key: string, toIndex: number, visibleKeys: readonly string[]): void
  resetSession(): void
  /** Set when a PUT fails. Cleared after a PUT that caught up with the local list. */
  error: Signal<string | null>
}

/**
 * One `accountId/roomId` entry. The account is a UUID, the room id starts
 * with `!` and is longer than that one character — the server's
 * `validate_space_order_entry`, which rejects the whole value otherwise.
 */
export function validSpaceOrderEntry(entry: string): boolean {
  const slash = entry.indexOf('/')
  if (slash <= 0) {
    return false
  }
  const accountId = entry.slice(0, slash)
  const roomId = entry.slice(slash + 1)
  return UUID.test(accountId) && roomId.startsWith('!') && roomId.length > 1
}

/** The closed `{ spaces }` value. Extra keys and malformed entries are rejected. */
export function parseSpaceOrder(value: unknown): string[] | null {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    return null
  }
  const record = value as Record<string, unknown>
  if (Object.keys(record).length !== 1 || !Array.isArray(record.spaces)) {
    return null
  }
  const spaces: string[] = []
  for (const entry of record.spaces) {
    if (typeof entry !== 'string' || !validSpaceOrderEntry(entry)) {
      return null
    }
    spaces.push(entry)
  }
  return spaces
}

/**
 * Instance-wide space rail order (ADR 0103). The displayed value stays
 * `settings.spaceOrder`, so the rail paints before GET returns. A 200
 * overwrites that local list unless a local edit is still unsaved or landed
 * during the request. A 404 uploads a non-empty local list once. A failed PUT
 * stays on screen and retries with backoff; the next GET must not paint the
 * older server list over it. Echoes of this device are ignored.
 *
 * The save loop keeps running while a refetch or a retry is in flight, so a
 * move that arrives then is queued on the same loop instead of being dropped
 * when that loop's promise is still the current one.
 */
export function createSpaceOrderStore(
  api: ApiClient,
  live: LiveConnection,
  deviceId: string,
  settings: {
    spaceOrder: Signal<string[]>
    moveSpace(
      key: string,
      toIndex: number,
      visibleKeys: readonly string[],
    ): void
  },
): SpaceOrderStore {
  let sessionGeneration = 0
  let requested = false
  let ready = false
  let revision = 0
  let hydration: Promise<void> | null = null
  let writeInFlight = false
  let siblingFrameDuringWrite = false
  let saveLoop: Promise<void> | null = null
  let queued: string[] | null = null
  /** True from a queued PUT until that body is what the server has stored. */
  let unsaved = false
  let saveFailures = 0
  const error = signal<string | null>(null)

  function currentSpaces(): string[] {
    return settings.spaceOrder.peek().filter(validSpaceOrderEntry)
  }

  function listsEqual(
    left: readonly string[],
    right: readonly string[],
  ): boolean {
    return (
      left.length === right.length &&
      left.every((entry, index) => entry === right[index])
    )
  }

  function applyServer(spaces: string[]): void {
    settings.spaceOrder.value = spaces
    revision += 1
  }

  function sleep(ms: number): Promise<void> {
    return new Promise((resolve) => {
      setTimeout(resolve, ms)
    })
  }

  async function put(spaces: string[], generation: number): Promise<boolean> {
    try {
      const result = await api.PUT('/v1/preferences/{key}', {
        params: { path: { key: SPACE_ORDER_KEY } },
        body: { device_id: deviceId, value: { spaces } },
      })
      if (generation !== sessionGeneration) {
        return false
      }
      if (result.error !== undefined || !result.response.ok) {
        return false
      }
      return true
    } catch {
      return generation === sessionGeneration ? false : false
    }
  }

  function enqueuePut(spaces: string[]): void {
    queued = spaces
    unsaved = true
    if (saveLoop !== null) {
      return
    }
    const generation = sessionGeneration
    writeInFlight = true
    siblingFrameDuringWrite = false
    const started = (async () => {
      try {
        while (generation === sessionGeneration) {
          if (queued !== null) {
            const body = queued
            queued = null
            const ok = await put(body, generation)
            if (generation !== sessionGeneration) {
              return
            }
            if (!ok) {
              error.value = 'Could not save space order'
              saveFailures += 1
              const delay = Math.min(
                SAVE_RETRY_MAX_MS,
                SAVE_RETRY_BASE_MS * 2 ** Math.min(saveFailures - 1, 5),
              )
              await sleep(delay)
              if (generation !== sessionGeneration) {
                return
              }
              if (queued === null) {
                queued = currentSpaces()
              }
              continue
            }
            if (queued === null && listsEqual(body, currentSpaces())) {
              unsaved = false
              saveFailures = 0
              if (error.peek() !== null) {
                error.value = null
              }
            }
            continue
          }
          if (siblingFrameDuringWrite) {
            siblingFrameDuringWrite = false
            await fetchPreference(generation)
            continue
          }
          break
        }
      } finally {
        if (generation === sessionGeneration) {
          writeInFlight = false
        }
      }
    })()
    saveLoop = started
    void started.finally(() => {
      if (saveLoop === started) {
        saveLoop = null
      }
    })
  }

  async function fetchPreference(generation: number): Promise<void> {
    const issued = revision
    const preserveLocal = () =>
      unsaved || revision !== issued || queued !== null
    try {
      const result = await api.GET('/v1/preferences/{key}', {
        params: { path: { key: SPACE_ORDER_KEY } },
      })
      if (generation !== sessionGeneration) {
        return
      }
      // `writeInFlight` is the wrong signal here. The trailing sibling
      // refetch runs inside the save loop, and it must still apply a list
      // the user has not moved. An unsaved edit, a move during this GET, or
      // a PUT already queued is what has to win.
      if (result.response.status === 404) {
        ready = true
        const local = currentSpaces()
        // An empty local list is "never set here". Uploading it would turn a
        // 404 into an explicit empty order and block a sibling that still
        // has the old key.
        if (local.length > 0) {
          enqueuePut(local)
        }
        return
      }
      if (result.error !== undefined || result.data === undefined) {
        ready = true
        if (preserveLocal()) {
          enqueuePut(currentSpaces())
        }
        return
      }
      ready = true
      if (preserveLocal()) {
        enqueuePut(currentSpaces())
        return
      }
      const parsed = parseSpaceOrder(result.data.data.value)
      if (parsed !== null) {
        applyServer(parsed)
      }
    } catch {
      if (generation !== sessionGeneration) {
        return
      }
      ready = true
      // A thrown GET is the same miss as an error response. A move made
      // while it was in flight has to be uploaded, or `ready` leaves it
      // local until the next move.
      if (preserveLocal()) {
        enqueuePut(currentSpaces())
      }
    }
  }

  live.subscribe((frame) => {
    const change = preferenceChange(frame)
    if (
      change === null ||
      change.key !== SPACE_ORDER_KEY ||
      change.deviceId === deviceId
    ) {
      return
    }
    const parsed = parseSpaceOrder(change.value)
    if (parsed === null) {
      return
    }
    if (writeInFlight) {
      siblingFrameDuringWrite = true
      return
    }
    applyServer(parsed)
    ready = true
  })

  effect(() => {
    if (live.reconnects.value === 0 || !requested) {
      return
    }
    const generation = sessionGeneration
    void fetchPreference(generation)
  })

  return {
    hydrate() {
      requested = true
      if (hydration !== null) {
        return hydration
      }
      const generation = sessionGeneration
      const started = fetchPreference(generation).finally(() => {
        if (hydration === started) {
          hydration = null
        }
      })
      hydration = started
      return started
    },
    move(key, toIndex, visibleKeys) {
      settings.moveSpace(key, toIndex, visibleKeys)
      revision += 1
      if (!ready) {
        return
      }
      enqueuePut(currentSpaces())
    },
    resetSession() {
      sessionGeneration += 1
      requested = false
      ready = false
      hydration = null
      writeInFlight = false
      siblingFrameDuringWrite = false
      queued = null
      unsaved = false
      saveFailures = 0
      saveLoop = null
      revision += 1
      if (error.peek() !== null) {
        error.value = null
      }
    },
    error,
  }
}
