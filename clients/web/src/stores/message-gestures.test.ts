import { signal } from '@preact/signals'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createApiClient } from '../api/client'
import { PREFERENCES_CHANGED, type LiveFrame } from '../api/frames'
import type { LiveConnection } from './live-connection'
import {
  assignMessageGestureAction,
  createMessageGestureStore,
  defaultMessageGestures,
  isSingleEmoji,
  parseMessageGestures,
  type MessageGesturePreferences,
} from './message-gestures'

const BASE_URL = 'http://axon.test'
const URL = `${BASE_URL}/v1/preferences/message_gestures`
const DEVICE_ID = 'device-this'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function preferenceResponse(value: MessageGesturePreferences) {
  return HttpResponse.json({
    data: {
      key: 'message_gestures',
      value,
      updated_at: '2026-09-14T00:00:00Z',
    },
  })
}

function harness() {
  const reconnects = signal(0)
  const listeners = new Set<(frame: LiveFrame) => void>()
  const live = {
    reconnects,
    subscribe(listener: (frame: LiveFrame) => void) {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
  } as unknown as LiveConnection
  const api = createApiClient(
    {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
  const store = createMessageGestureStore(api, live, DEVICE_ID)
  return {
    store,
    reconnects,
    emit(value: unknown, deviceId = 'device-other') {
      listeners.forEach((listener) =>
        listener({
          type: PREFERENCES_CHANGED,
          accountId: '00000000-0000-0000-0000-000000000000',
          payload: {
            key: 'message_gestures',
            value,
            device_id: deviceId,
          },
        }),
      )
    },
  }
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0))

describe('parseMessageGestures', () => {
  it('accepts the complete v1 shape and rejects duplicates or extensions', () => {
    expect(parseMessageGestures(defaultMessageGestures())).toEqual(
      defaultMessageGestures(),
    )
    expect(
      parseMessageGestures({
        ...defaultMessageGestures(),
        bindings: {
          double_tap: 'reply',
          touch_and_hold: 'reply',
          swipe_left: null,
        },
      }),
    ).toBeNull()
    expect(
      parseMessageGestures({
        ...defaultMessageGestures(),
        swipe_right: 'edit',
      }),
    ).toBeNull()
    expect(
      parseMessageGestures({
        schema_version: 1,
        bindings: { double_tap: 'react', touch_and_hold: 'thread' },
        reaction_emoji: '👍',
      }),
    ).toBeNull()
  })

  it('accepts one emoji grapheme, including flags and joined families', () => {
    expect(isSingleEmoji('👍')).toBe(true)
    expect(isSingleEmoji('🇨🇦')).toBe(true)
    expect(isSingleEmoji('👨‍👩‍👧‍👦')).toBe(true)
    expect(isSingleEmoji('1️⃣')).toBe(true)
    expect(isSingleEmoji('hello')).toBe(false)
    expect(isSingleEmoji('1')).toBe(false)
    expect(isSingleEmoji('🏽')).toBe(false)
    expect(isSingleEmoji('👍🎉')).toBe(false)
  })
})

describe('assignMessageGestureAction', () => {
  it('swaps an existing assignment with the selected gesture action', () => {
    const current: MessageGesturePreferences = {
      schema_version: 1,
      bindings: {
        double_tap: 'edit',
        touch_and_hold: 'thread',
        swipe_left: 'reply',
      },
      reaction_emoji: '👍',
    }

    expect(assignMessageGestureAction(current, 'double_tap', 'thread')).toEqual(
      {
        value: {
          ...current,
          bindings: {
            double_tap: 'thread',
            touch_and_hold: 'edit',
            swipe_left: 'reply',
          },
        },
        displaced: 'touch_and_hold',
        replacement: 'edit',
      },
    )
    expect(current.bindings).toEqual({
      double_tap: 'edit',
      touch_and_hold: 'thread',
      swipe_left: 'reply',
    })
  })

  it('turns off the displaced gesture when the selected gesture was off', () => {
    const current: MessageGesturePreferences = {
      ...defaultMessageGestures(),
      bindings: {
        double_tap: null,
        touch_and_hold: 'thread',
        swipe_left: 'reply',
      },
    }

    expect(assignMessageGestureAction(current, 'double_tap', 'thread')).toEqual(
      {
        value: {
          ...current,
          bindings: {
            double_tap: 'thread',
            touch_and_hold: null,
            swipe_left: 'reply',
          },
        },
        displaced: 'touch_and_hold',
        replacement: null,
      },
    )
  })
})

describe('createMessageGestureStore', () => {
  it('uses the default preset without writing when the preference is absent', async () => {
    let writes = 0
    server.use(
      http.get(URL, () =>
        HttpResponse.json(
          { error: { code: 'not_found', message: 'preference not found' } },
          { status: 404 },
        ),
      ),
      http.put(URL, () => {
        writes += 1
        return HttpResponse.json({ data: { updated_at: 'now' } })
      }),
    )
    const { store } = harness()

    await store.hydrate()

    expect(store.preferences.value).toEqual(defaultMessageGestures())
    expect(store.status.value).toBe('ready')
    expect(writes).toBe(0)
  })

  it('uses the default preset when the stored value is malformed', async () => {
    server.use(
      http.get(URL, () =>
        HttpResponse.json({
          data: {
            key: 'message_gestures',
            value: { schema_version: 99 },
            updated_at: 'now',
          },
        }),
      ),
    )
    const { store } = harness()

    await store.hydrate()

    expect(store.preferences.value).toEqual(defaultMessageGestures())
    expect(store.status.value).toBe('ready')
    expect(store.error.value).toBe('message gesture preferences are not valid')
  })

  it('writes the whole value with this device id', async () => {
    let requestBody: unknown
    server.use(
      http.put(URL, async ({ request }) => {
        requestBody = await request.json()
        return HttpResponse.json({ data: { updated_at: 'now' } })
      }),
    )
    const { store } = harness()
    const value: MessageGesturePreferences = {
      schema_version: 1,
      bindings: {
        double_tap: 'edit',
        touch_and_hold: null,
        swipe_left: 'delete',
      },
      reaction_emoji: '🚀',
    }

    expect(await store.save(value)).toBe(true)

    expect(requestBody).toEqual({ device_id: DEVICE_ID, value })
    expect(store.preferences.value).toEqual(value)
  })

  it('applies changes immediately and coalesces edits made during a write', async () => {
    let releaseFirst!: () => void
    const firstWrite = new Promise<void>((resolve) => {
      releaseFirst = resolve
    })
    const writes: MessageGesturePreferences[] = []
    server.use(
      http.put(URL, async ({ request }) => {
        const body = (await request.json()) as {
          value: MessageGesturePreferences
        }
        writes.push(body.value)
        if (writes.length === 1) {
          await firstWrite
        }
        return HttpResponse.json({ data: { updated_at: 'now' } })
      }),
    )
    const { store } = harness()
    const first = {
      ...defaultMessageGestures(),
      reaction_emoji: '🎉',
    }
    const intermediate = {
      ...first,
      reaction_emoji: '❤️',
    }
    const latest = {
      ...intermediate,
      reaction_emoji: '🚀',
    }

    const firstResult = store.save(first)
    expect(store.preferences.value).toEqual(first)
    expect(store.saving.value).toBe(true)
    const intermediateResult = store.save(intermediate)
    const latestResult = store.save(latest)
    expect(store.preferences.value).toEqual(latest)

    releaseFirst()
    expect(
      await Promise.all([firstResult, intermediateResult, latestResult]),
    ).toEqual([true, true, true])
    expect(writes).toEqual([first, latest])
    expect(store.preferences.value).toEqual(latest)
    expect(store.saving.value).toBe(false)
  })

  it('keeps an optimistic local edit visible while reconciling a sibling write', async () => {
    let releaseWrite!: () => void
    const writeGate = new Promise<void>((resolve) => {
      releaseWrite = resolve
    })
    const local = {
      ...defaultMessageGestures(),
      reaction_emoji: '🚀',
    }
    const sibling = {
      ...defaultMessageGestures(),
      reaction_emoji: '🎉',
    }
    server.use(
      http.put(URL, async () => {
        await writeGate
        return HttpResponse.json({ data: { updated_at: 'now' } })
      }),
      http.get(URL, () => preferenceResponse(sibling)),
    )
    const { store, emit } = harness()

    const saving = store.save(local)
    emit(sibling)
    expect(store.preferences.value).toEqual(local)

    releaseWrite()
    expect(await saving).toBe(true)
    expect(store.preferences.value).toEqual(sibling)
  })

  it('writes a newer local edit that arrives during sibling reconciliation', async () => {
    let releaseFirstWrite!: () => void
    const firstWrite = new Promise<void>((resolve) => {
      releaseFirstWrite = resolve
    })
    let noteGetStarted!: () => void
    const getStarted = new Promise<void>((resolve) => {
      noteGetStarted = resolve
    })
    let releaseGet!: () => void
    const getGate = new Promise<void>((resolve) => {
      releaseGet = resolve
    })
    const writes: MessageGesturePreferences[] = []
    const first = {
      ...defaultMessageGestures(),
      reaction_emoji: '🚀',
    }
    const sibling = {
      ...defaultMessageGestures(),
      reaction_emoji: '🎉',
    }
    const latest = {
      ...defaultMessageGestures(),
      reaction_emoji: '❤️',
    }
    server.use(
      http.put(URL, async ({ request }) => {
        const body = (await request.json()) as {
          value: MessageGesturePreferences
        }
        writes.push(body.value)
        if (writes.length === 1) {
          await firstWrite
        }
        return HttpResponse.json({ data: { updated_at: 'now' } })
      }),
      http.get(URL, async () => {
        noteGetStarted()
        await getGate
        return preferenceResponse(sibling)
      }),
    )
    const { store, emit } = harness()

    const firstResult = store.save(first)
    emit(sibling)
    releaseFirstWrite()
    await getStarted
    const latestResult = store.save(latest)
    expect(store.preferences.value).toEqual(latest)
    releaseGet()

    expect(await Promise.all([firstResult, latestResult])).toEqual([true, true])
    expect(writes).toEqual([first, latest])
    expect(store.preferences.value).toEqual(latest)
  })

  it('keeps a failed optimistic edit available for retry', async () => {
    let fail = true
    let writes = 0
    server.use(
      http.put(URL, () => {
        writes += 1
        return fail
          ? HttpResponse.json(
              { error: { code: 'unavailable', message: 'try again' } },
              { status: 503 },
            )
          : HttpResponse.json({ data: { updated_at: 'now' } })
      }),
    )
    const { store } = harness()
    const desired = {
      ...defaultMessageGestures(),
      reaction_emoji: '🚀',
    }

    expect(await store.save(desired)).toBe(false)
    expect(store.preferences.value).toEqual(desired)
    expect(store.error.value).toBe('try again')

    fail = false
    expect(await store.save(desired)).toBe(true)
    expect(store.error.value).toBeNull()
    expect(store.preferences.value).toEqual(desired)
    expect(writes).toBe(2)
  })

  it('applies sibling frames, ignores echoes, and refetches after reconnect', async () => {
    let reads = 0
    let serverValue = defaultMessageGestures()
    server.use(
      http.get(URL, () => {
        reads += 1
        return preferenceResponse(serverValue)
      }),
    )
    const { store, emit, reconnects } = harness()
    await store.hydrate()
    const sibling: MessageGesturePreferences = {
      schema_version: 1,
      bindings: {
        double_tap: 'reply',
        touch_and_hold: 'react',
        swipe_left: null,
      },
      reaction_emoji: '🎉',
    }

    emit(sibling)
    expect(store.preferences.value).toEqual(sibling)
    emit(defaultMessageGestures(), DEVICE_ID)
    expect(store.preferences.value).toEqual(sibling)

    serverValue = {
      ...sibling,
      reaction_emoji: '❤️',
    }
    reconnects.value += 1
    await settle()

    expect(reads).toBe(2)
    expect(store.preferences.value).toEqual(serverValue)
  })

  it('does not apply a GET that completes after the session resets', async () => {
    let release: (() => void) | undefined
    server.use(
      http.get(URL, async () => {
        await new Promise<void>((resolve) => {
          release = resolve
        })
        return preferenceResponse(defaultMessageGestures())
      }),
    )
    const { store } = harness()

    const hydration = store.hydrate()
    await settle()
    store.resetSession()
    release?.()
    await hydration

    expect(store.preferences.value).toBeNull()
    expect(store.status.value).toBe('idle')
  })
})
