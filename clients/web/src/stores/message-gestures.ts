import { computed, effect, signal, type ReadonlySignal } from '@preact/signals'
import { apiErrorMessage, type ApiClient } from '../api/client'
import { preferenceChange } from '../api/frames'
import { hasEmojiCandidate } from '../emoji'
import type { LiveConnection } from './live-connection'

export const MESSAGE_GESTURES_KEY = 'message_gestures'

export const MESSAGE_GESTURE_ACTIONS = [
  'reply',
  'thread',
  'react',
  'edit',
  'delete',
] as const

export type MessageGestureAction = (typeof MESSAGE_GESTURE_ACTIONS)[number]
export type MessageGesture = 'double_tap' | 'touch_and_hold' | 'swipe_left'

export interface MessageGesturePreferences {
  schema_version: 1
  bindings: Record<MessageGesture, MessageGestureAction | null>
  reaction_emoji: string
}

export type MessageGesturePreferenceStatus =
  'idle' | 'loading' | 'ready' | 'error'

export interface MessageGestureStore {
  /** Null until GET establishes the stored value or the default-on-404. */
  readonly preferences: ReadonlySignal<MessageGesturePreferences | null>
  readonly status: ReadonlySignal<MessageGesturePreferenceStatus>
  readonly error: ReadonlySignal<string | null>
  readonly saving: ReadonlySignal<boolean>
  /** Increments for every accepted GET, PUT, or sibling-device frame. */
  readonly revision: ReadonlySignal<number>
  hydrate(): Promise<void>
  save(value: MessageGesturePreferences): Promise<boolean>
  resetSession(): void
}

export function defaultMessageGestures(): MessageGesturePreferences {
  return {
    schema_version: 1,
    bindings: {
      double_tap: 'react',
      touch_and_hold: 'thread',
      swipe_left: 'reply',
    },
    reaction_emoji: '👍',
  }
}

export function assignMessageGestureAction(
  value: MessageGesturePreferences,
  gesture: MessageGesture,
  action: MessageGestureAction | null,
): {
  value: MessageGesturePreferences
  displaced: MessageGesture | null
  replacement: MessageGestureAction | null
} {
  const replacement = value.bindings[gesture]
  const displaced =
    action === null
      ? null
      : (GESTURES.find(
          (other) => other !== gesture && value.bindings[other] === action,
        ) ?? null)
  const bindings = { ...value.bindings, [gesture]: action }
  if (displaced !== null) {
    bindings[displaced] = replacement
  }
  return {
    value: { ...value, bindings },
    displaced,
    replacement,
  }
}

const ACTIONS = new Set<string>(MESSAGE_GESTURE_ACTIONS)
const GESTURES: readonly MessageGesture[] = [
  'double_tap',
  'touch_and_hold',
  'swipe_left',
]
/** The server accepts exactly one Unicode emoji grapheme, up to 64 bytes. */
export function isSingleEmoji(value: string): boolean {
  if (
    value === '' ||
    new TextEncoder().encode(value).byteLength > 64 ||
    !hasEmojiCandidate(value)
  ) {
    return false
  }
  return (
    [
      ...new Intl.Segmenter(undefined, { granularity: 'grapheme' }).segment(
        value,
      ),
    ].length === 1
  )
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function hasExactlyKeys(
  value: Record<string, unknown>,
  expected: readonly string[],
): boolean {
  const actual = Object.keys(value).sort()
  const sortedExpected = [...expected].sort()
  return (
    actual.length === sortedExpected.length &&
    actual.every((key, index) => key === sortedExpected[index])
  )
}

/** Parse the server's closed v1 value; malformed live frames are ignored. */
export function parseMessageGestures(
  value: unknown,
): MessageGesturePreferences | null {
  if (
    !isRecord(value) ||
    !hasExactlyKeys(value, ['schema_version', 'bindings', 'reaction_emoji']) ||
    value.schema_version !== 1 ||
    !isRecord(value.bindings) ||
    !hasExactlyKeys(value.bindings, GESTURES) ||
    typeof value.reaction_emoji !== 'string' ||
    !isSingleEmoji(value.reaction_emoji)
  ) {
    return null
  }

  const bindings = {} as MessageGesturePreferences['bindings']
  const seen = new Set<MessageGestureAction>()
  for (const gesture of GESTURES) {
    const action = value.bindings[gesture]
    if (
      action !== null &&
      (typeof action !== 'string' || !ACTIONS.has(action))
    ) {
      return null
    }
    if (action !== null) {
      const typed = action as MessageGestureAction
      if (seen.has(typed)) {
        return null
      }
      seen.add(typed)
      bindings[gesture] = typed
    } else {
      bindings[gesture] = null
    }
  }
  return {
    schema_version: 1,
    bindings,
    reaction_emoji: value.reaction_emoji,
  }
}

function clonePreferences(
  value: MessageGesturePreferences,
): MessageGesturePreferences {
  return {
    schema_version: 1,
    bindings: { ...value.bindings },
    reaction_emoji: value.reaction_emoji,
  }
}

/**
 * Axon-wide message gesture preferences (ADR 0104).
 *
 * GET is the reconnect source of truth; `preferences.changed` supplies the
 * live path. Responses are ordered against frames and local saves with a
 * revision counter so an older in-flight GET cannot erase a newer value.
 */
export function createMessageGestureStore(
  api: ApiClient,
  live: LiveConnection,
  deviceId: string,
): MessageGestureStore {
  const preferences = signal<MessageGesturePreferences | null>(null)
  const status = signal<MessageGesturePreferenceStatus>('idle')
  const error = signal<string | null>(null)
  const saving = signal(false)
  const revision = signal(0)
  let sessionGeneration = 0
  let requested = false
  let hydration: Promise<void> | null = null
  let writeInFlight = false
  let siblingFrameDuringWrite = false

  function apply(value: MessageGesturePreferences): void {
    preferences.value = clonePreferences(value)
    status.value = 'ready'
    error.value = null
    revision.value += 1
  }

  async function fetchPreference(): Promise<void> {
    const generation = sessionGeneration
    const issuedAtRevision = revision.peek()
    if (preferences.peek() === null) {
      status.value = 'loading'
    }
    try {
      const result = await api.GET('/v1/preferences/{key}', {
        params: { path: { key: MESSAGE_GESTURES_KEY } },
      })
      if (generation !== sessionGeneration) {
        return
      }
      if (revision.peek() !== issuedAtRevision) {
        return
      }
      if (result.response.status === 404) {
        apply(defaultMessageGestures())
        return
      }
      if (result.error !== undefined || result.data === undefined) {
        error.value =
          result.error === undefined
            ? 'message gesture preferences failed'
            : apiErrorMessage(result.error)
        status.value = preferences.peek() === null ? 'error' : 'ready'
        return
      }
      const parsed = parseMessageGestures(result.data.data.value)
      if (parsed === null) {
        apply(defaultMessageGestures())
        error.value = 'message gesture preferences are not valid'
        return
      }
      apply(parsed)
    } catch (cause) {
      if (generation !== sessionGeneration) {
        return
      }
      error.value = cause instanceof Error ? cause.message : String(cause)
      status.value = preferences.peek() === null ? 'error' : 'ready'
    }
  }

  async function hydrate(): Promise<void> {
    requested = true
    if (hydration !== null) {
      return hydration
    }
    const started = fetchPreference().finally(() => {
      if (hydration === started) {
        hydration = null
      }
    })
    hydration = started
    return started
  }

  live.subscribe((frame) => {
    const change = preferenceChange(frame)
    if (
      change === null ||
      change.key !== MESSAGE_GESTURES_KEY ||
      change.deviceId === deviceId
    ) {
      return
    }
    const parsed = parseMessageGestures(change.value)
    if (parsed === null) {
      return
    }
    if (writeInFlight) {
      siblingFrameDuringWrite = true
    }
    apply(parsed)
  })

  effect(() => {
    if (live.reconnects.value === 0 || !requested) {
      return
    }
    void hydrate()
  })

  return {
    preferences: computed(() => preferences.value),
    status: computed(() => status.value),
    error,
    saving: computed(() => saving.value),
    revision: computed(() => revision.value),
    hydrate,
    async save(value) {
      const parsed = parseMessageGestures(value)
      if (parsed === null || saving.peek()) {
        return false
      }
      const generation = sessionGeneration
      saving.value = true
      writeInFlight = true
      siblingFrameDuringWrite = false
      try {
        const result = await api.PUT('/v1/preferences/{key}', {
          params: { path: { key: MESSAGE_GESTURES_KEY } },
          body: { device_id: deviceId, value: parsed },
        })
        if (generation !== sessionGeneration) {
          return false
        }
        if (result.error !== undefined || !result.response.ok) {
          error.value =
            result.error === undefined
              ? 'message gesture preferences failed'
              : apiErrorMessage(result.error)
          return false
        }
        if (siblingFrameDuringWrite) {
          await fetchPreference()
        } else {
          apply(parsed)
        }
        return true
      } catch (cause) {
        if (generation === sessionGeneration) {
          error.value = cause instanceof Error ? cause.message : String(cause)
        }
        return false
      } finally {
        if (generation === sessionGeneration) {
          writeInFlight = false
          saving.value = false
        }
      }
    },
    resetSession() {
      sessionGeneration += 1
      requested = false
      hydration = null
      writeInFlight = false
      siblingFrameDuringWrite = false
      if (
        preferences.peek() === null &&
        status.peek() === 'idle' &&
        error.peek() === null &&
        !saving.peek()
      ) {
        return
      }
      preferences.value = null
      status.value = 'idle'
      error.value = null
      saving.value = false
      revision.value += 1
    },
  }
}
