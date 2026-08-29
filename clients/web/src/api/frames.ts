import type { components } from './schema'

export type EventDto = components['schemas']['EventDto']

/**
 * A decoded `/v1/ws` frame: the wire envelope `{ type, account_id, payload }`
 * (`crates/axon-api/src/ws.rs`), with `account_id` surfaced as `accountId`.
 *
 * The `type` tag is kept as an opaque string so the router (M-W6, ADR 0061)
 * can dispatch by tag and silently ignore kinds this client predates — a new
 * server frame type reaches no handler until one is added, rather than
 * breaking decode. Known tags today: `timeline.event`, `verification.*`,
 * `sender_trust.violation`, `device_state.changed`, `ephemeral.passthrough`,
 * `unread_counts.changed`, `invite.added`, `invite.removed`.
 */
export interface LiveFrame {
  /** Namespaced tag, e.g. `timeline.event`. */
  type: string
  /** The account the frame pertains to; clients self-filter on it (ADR 0020). */
  accountId: string
  /** Tag-specific body; narrow it with the tag's accessor before use. */
  payload: unknown
}

/** The `type` tag for a live timeline event (`ws.rs` `TIMELINE_EVENT`). */
export const TIMELINE_EVENT = 'timeline.event'

/** The `type` tag for a per-device state change (`ws.rs` `DEVICE_STATE_CHANGED`). */
export const DEVICE_STATE_CHANGED = 'device_state.changed'

/** The `type` tag for raw allowlisted ephemeral Matrix events (ADR 0056). */
export const EPHEMERAL_PASSTHROUGH = 'ephemeral.passthrough'

/** The `type` tag for server-derived per-room unread counts (ADR 0070). */
export const UNREAD_COUNTS_CHANGED = 'unread_counts.changed'

/** The `type` tag for a newly persisted pending invite (ADR 0091). */
export const INVITE_ADDED = 'invite.added'

/** The `type` tag for a pending invite that is no longer pending (ADR 0091). */
export const INVITE_REMOVED = 'invite.removed'

/**
 * The payload of a `device_state.changed` frame (M12, ADR 0048): the writing
 * `deviceId`, the `namespace`, and the written `entries` (a `null` value is a
 * delete/tombstone). Consumers drop frames whose `deviceId` is their own
 * (echo suppression) and apply the rest to their cache.
 */
export interface DeviceStateChange {
  deviceId: string
  namespace: string
  entries: Record<string, unknown>
}

/**
 * The payload of an `ephemeral.passthrough` frame. `roomId` is optional on the
 * wire because future account-scoped signals such as presence may have none;
 * the current room UI consumes only room-scoped `m.typing` and `m.receipt`.
 */
export interface EphemeralPassthrough {
  roomId: string | null
  eventType: string
  content: unknown
}

/** The payload of an `unread_counts.changed` frame (ADR 0070). */
export interface UnreadCountsChange {
  roomId: string
  notificationCount: number
  highlightCount: number
}

/**
 * Parse one raw WS message into a [`LiveFrame`], or `null` when it is not a
 * well-formed envelope (non-JSON, not an object, or a missing/non-string
 * `type` or `account_id`). Malformed frames are dropped, never thrown: one
 * unreadable frame must not tear down the socket.
 */
export function decodeFrame(raw: string): LiveFrame | null {
  let value: unknown
  try {
    value = JSON.parse(raw)
  } catch {
    return null
  }
  if (typeof value !== 'object' || value === null) {
    return null
  }
  const {
    type,
    account_id: accountId,
    payload,
  } = value as Record<string, unknown>
  if (typeof type !== 'string' || typeof accountId !== 'string') {
    return null
  }
  return { type, accountId, payload }
}

/**
 * The `EventDto` payload of a `timeline.event` frame, or `null` for any other
 * tag (or a payload that isn't an object). The event carries the read API's
 * shape — decrypted, with a `sender_trust` snapshot and its own `room_id` — so
 * a consumer can attribute it to a room without extra lookup.
 */
export function timelineEvent(frame: LiveFrame): EventDto | null {
  if (frame.type !== TIMELINE_EVENT) {
    return null
  }
  if (typeof frame.payload !== 'object' || frame.payload === null) {
    return null
  }
  return frame.payload as EventDto
}

/**
 * The [`DeviceStateChange`] of a `device_state.changed` frame, or `null` for
 * any other tag (or a malformed payload). The frame's account is on the
 * envelope (`frame.accountId`), so a consumer keys the change by
 * `(accountId, namespace, key)`.
 */
export function deviceStateChange(frame: LiveFrame): DeviceStateChange | null {
  if (frame.type !== DEVICE_STATE_CHANGED) {
    return null
  }
  if (typeof frame.payload !== 'object' || frame.payload === null) {
    return null
  }
  const {
    device_id: deviceId,
    namespace,
    entries,
  } = frame.payload as Record<string, unknown>
  if (
    typeof deviceId !== 'string' ||
    typeof namespace !== 'string' ||
    typeof entries !== 'object' ||
    entries === null
  ) {
    return null
  }
  return { deviceId, namespace, entries: entries as Record<string, unknown> }
}

/**
 * The raw Matrix ephemeral payload forwarded by the server, or `null` for any
 * other tag or malformed payload. The frame's account is on the envelope.
 */
export function ephemeralPassthrough(
  frame: LiveFrame,
): EphemeralPassthrough | null {
  if (frame.type !== EPHEMERAL_PASSTHROUGH) {
    return null
  }
  if (typeof frame.payload !== 'object' || frame.payload === null) {
    return null
  }
  const {
    room_id: roomId,
    event_type: eventType,
    content,
  } = frame.payload as Record<string, unknown>
  if (
    !(typeof roomId === 'string' || roomId === undefined || roomId === null) ||
    typeof eventType !== 'string'
  ) {
    return null
  }
  return { roomId: roomId ?? null, eventType, content }
}

/**
 * The server-derived unread counts for one room, or `null` for any other tag
 * or malformed payload. The frame's account is on the envelope.
 */
export function unreadCountsChange(
  frame: LiveFrame,
): UnreadCountsChange | null {
  if (frame.type !== UNREAD_COUNTS_CHANGED) {
    return null
  }
  if (typeof frame.payload !== 'object' || frame.payload === null) {
    return null
  }
  const {
    room_id: roomId,
    notification_count: notificationCount,
    highlight_count: highlightCount,
  } = frame.payload as Record<string, unknown>
  if (
    typeof roomId !== 'string' ||
    !isCount(notificationCount) ||
    !isCount(highlightCount)
  ) {
    return null
  }
  return { roomId, notificationCount, highlightCount }
}

function isCount(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0
}

function optionalString(value: unknown): string | null {
  return typeof value === 'string' ? value : null
}

/**
 * The payload of an `invite.added` frame, or `null` for any other tag or a
 * malformed body. Field names match `InviteDto`.
 */
export function inviteAdded(frame: LiveFrame): InviteAdded | null {
  if (frame.type !== INVITE_ADDED) {
    return null
  }
  if (typeof frame.payload !== 'object' || frame.payload === null) {
    return null
  }
  const payload = frame.payload as Record<string, unknown>
  if (
    typeof payload.account_id !== 'string' ||
    typeof payload.account_user_id !== 'string' ||
    typeof payload.room_id !== 'string' ||
    typeof payload.inviter_user_id !== 'string' ||
    typeof payload.is_direct !== 'boolean' ||
    typeof payload.encrypted !== 'boolean' ||
    typeof payload.invited_at !== 'string'
  ) {
    return null
  }
  return {
    account_id: payload.account_id,
    account_user_id: payload.account_user_id,
    room_id: payload.room_id,
    name: optionalString(payload.name),
    avatar_url: optionalString(payload.avatar_url),
    topic: optionalString(payload.topic),
    canonical_alias: optionalString(payload.canonical_alias),
    room_type: optionalString(payload.room_type),
    inviter_user_id: payload.inviter_user_id,
    inviter_display_name: optionalString(payload.inviter_display_name),
    is_direct: payload.is_direct,
    encrypted: payload.encrypted,
    invited_at: payload.invited_at,
  }
}

/** The payload of an `invite.removed` frame. Account is on the envelope. */
export interface InviteRemoved {
  roomId: string
}

export type InviteAdded = {
  account_id: string
  account_user_id: string
  room_id: string
  name: string | null
  avatar_url: string | null
  topic: string | null
  canonical_alias: string | null
  room_type: string | null
  inviter_user_id: string
  inviter_display_name: string | null
  is_direct: boolean
  encrypted: boolean
  invited_at: string
}

export function inviteRemoved(frame: LiveFrame): InviteRemoved | null {
  if (frame.type !== INVITE_REMOVED) {
    return null
  }
  if (typeof frame.payload !== 'object' || frame.payload === null) {
    return null
  }
  const { room_id: roomId } = frame.payload as Record<string, unknown>
  return typeof roomId === 'string' ? { roomId } : null
}

/** The `type` tag for a peer-initiated SAS request (ADR 0027). */
export const VERIFICATION_REQUESTED = 'verification.requested'

/** The `type` tag for SAS emoji/decimals becoming available. */
export const VERIFICATION_SAS = 'verification.sas'

/** The `type` tag for a completed SAS flow. */
export const VERIFICATION_DONE = 'verification.done'

/** The `type` tag for a cancelled SAS flow. */
export const VERIFICATION_CANCELLED = 'verification.cancelled'

export type VerificationFrameKind = 'requested' | 'sas' | 'done' | 'cancelled'

export interface VerificationEmoji {
  symbol: string
  description: string
}

/**
 * Decoded `verification.*` payload. `deviceId` is null for cross-user
 * inbound (wire `device_id` omitted or null). `emoji`/`decimals` are only
 * populated on `verification.sas`; `reason` only on `verification.cancelled`.
 */
export interface VerificationFramePayload {
  flowId: string
  userId: string
  deviceId: string | null
  emoji: VerificationEmoji[] | null
  decimals: [number, number, number] | null
  reason: string | null
}

const VERIFICATION_KIND: Record<string, VerificationFrameKind> = {
  [VERIFICATION_REQUESTED]: 'requested',
  [VERIFICATION_SAS]: 'sas',
  [VERIFICATION_DONE]: 'done',
  [VERIFICATION_CANCELLED]: 'cancelled',
}

function optionalStringOrNull(value: unknown): string | null | false {
  if (value === undefined || value === null) {
    return null
  }
  return typeof value === 'string' ? value : false
}

function decodeFrameEmoji(value: unknown): VerificationEmoji[] | null | false {
  if (value === undefined || value === null) {
    return null
  }
  if (!Array.isArray(value)) {
    return false
  }
  const emoji: VerificationEmoji[] = []
  for (const item of value) {
    if (typeof item !== 'object' || item === null) {
      return false
    }
    const { symbol, description } = item as Record<string, unknown>
    if (typeof symbol !== 'string' || typeof description !== 'string') {
      return false
    }
    emoji.push({ symbol, description })
  }
  return emoji
}

function decodeFrameDecimals(
  value: unknown,
): [number, number, number] | null | false {
  if (value === undefined || value === null) {
    return null
  }
  if (
    !Array.isArray(value) ||
    value.length !== 3 ||
    !value.every(
      (entry) => typeof entry === 'number' && Number.isSafeInteger(entry),
    )
  ) {
    return false
  }
  return [value[0], value[1], value[2]]
}

/**
 * A `verification.*` frame, or `null` for any other tag or a malformed
 * payload. One bad frame must not tear the socket down (same as `inviteAdded`).
 */
export function verificationFrame(
  frame: LiveFrame,
): { kind: VerificationFrameKind; payload: VerificationFramePayload } | null {
  const kind = VERIFICATION_KIND[frame.type]
  if (kind === undefined) {
    return null
  }
  if (typeof frame.payload !== 'object' || frame.payload === null) {
    return null
  }
  const payload = frame.payload as Record<string, unknown>
  if (
    typeof payload.flow_id !== 'string' ||
    typeof payload.user_id !== 'string'
  ) {
    return null
  }
  const deviceId = optionalStringOrNull(payload.device_id)
  if (deviceId === false) {
    return null
  }
  const emoji = decodeFrameEmoji(payload.emoji)
  if (emoji === false) {
    return null
  }
  const decimals = decodeFrameDecimals(payload.decimals)
  if (decimals === false) {
    return null
  }
  const reason = optionalStringOrNull(payload.reason)
  if (reason === false) {
    return null
  }
  return {
    kind,
    payload: {
      flowId: payload.flow_id,
      userId: payload.user_id,
      deviceId,
      emoji,
      decimals,
      reason,
    },
  }
}
