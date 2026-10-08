/**
 * Local message notifications (issue 606).
 *
 * The shell and the browser share the decision of what a tap means and the
 * queue that holds a tap until the signed-in shell can route it. Posting is
 * the platform's job: the shell invokes the notification plugin, and the
 * browser may use the Web Notification API. Neither path belongs in here.
 */

/** How long a message preview may be, in characters, before it is clipped. */
const PREVIEW_LIMIT = 140

/** How many recent taps we can still resolve after the page reloads. */
const MAX_TARGETS = 40

const TARGETS_KEY = 'axon.notification-targets'

/** Where a notification tap should land. */
export interface NotificationClick {
  accountId: string
  roomId: string
  /** The message itself, when the post carried one. */
  eventId: string | null
  /** Set when the message is a thread reply, so the tap opens that thread. */
  threadRootId: string | null
}

/** What `Platform.notify` posts. The id that round-trips a tap is the shell's. */
export interface MessageNotification {
  title: string
  body: string
  accountId: string
  roomId: string
  eventId: string
  threadRootId: string | null
}

/**
 * What a permission check can tell Settings.
 *
 * `unsupported` means this page cannot show a notification at all, which is
 * distinct from the user having blocked one. Android Chrome and an iOS
 * home-screen web app expose `Notification` but throw on `new Notification`.
 */
export type NotificationPermissionState =
  'granted' | 'denied' | 'default' | 'unsupported'

/** A posted notification remembered so a tap that omits `extra` can still route. */
export interface NotificationTarget {
  id: number
  accountId: string
  roomId: string
  eventId: string | null
  threadRootId: string | null
}

/**
 * `sender: body`, clipped. An empty body is just the sender. With neither,
 * a fixed label, so the toast is never blank.
 */
export function messageNotificationBody(
  sender: string,
  body: string | null | undefined,
): string {
  const text = (body ?? '').replace(/\s+/g, ' ').trim()
  const who = sender.trim()
  const clipped =
    text.length > PREVIEW_LIMIT ? `${text.slice(0, PREVIEW_LIMIT - 1)}…` : text
  if (who === '') {
    return clipped === '' ? 'New message' : clipped
  }
  if (clipped === '') {
    return who
  }
  return `${who}: ${clipped}`
}

type ClickHandler = (click: NotificationClick) => void

const clickHandlers = new Set<ClickHandler>()
let pendingClick: NotificationClick | null = null

/**
 * Hand a tap to whoever is listening. With nobody listening yet — the page
 * has received the tap but has not reached the signed-in shell — keep the
 * latest one.
 *
 * This does not recover a tap the plugin fired before the page listened.
 * Android does that during plugin load, and an iOS cold start drops the
 * event when its in-memory map is empty. Desktop 2.5 does not emit a tap.
 */
export function deliverNotificationClick(click: NotificationClick): void {
  if (clickHandlers.size === 0) {
    pendingClick = click
    return
  }
  for (const handler of clickHandlers) {
    handler(click)
  }
}

/** Subscribe to taps. A tap that arrived earlier is delivered immediately. */
export function subscribeNotificationClicks(handler: ClickHandler): () => void {
  clickHandlers.add(handler)
  if (pendingClick !== null) {
    const click = pendingClick
    pendingClick = null
    handler(click)
  }
  return () => {
    clickHandlers.delete(handler)
  }
}

/** Remember a post and return the id to put on it. */
export function rememberNotificationTarget(
  storage: Storage,
  target: {
    accountId: string
    roomId: string
    eventId: string
    threadRootId: string | null
  },
): number {
  const targets = readNotificationTargets(storage)
  const id = unusedId(targets)
  const next = [
    ...targets,
    {
      id,
      accountId: target.accountId,
      roomId: target.roomId,
      eventId: target.eventId,
      threadRootId: target.threadRootId,
    },
  ].slice(-MAX_TARGETS)
  try {
    storage.setItem(TARGETS_KEY, JSON.stringify(next))
  } catch {
    // A full store must not swallow the toast. A tap can still use `extra`.
  }
  return id
}

/** The posts this storage can still route, newest last. */
export function readNotificationTargets(
  storage: Storage,
): NotificationTarget[] {
  let raw: string | null
  try {
    raw = storage.getItem(TARGETS_KEY)
  } catch {
    return []
  }
  if (raw === null) {
    return []
  }
  let parsed: unknown
  try {
    parsed = JSON.parse(raw) as unknown
  } catch {
    return []
  }
  if (!Array.isArray(parsed)) {
    return []
  }
  const targets: NotificationTarget[] = []
  for (const item of parsed) {
    const target = asTarget(item)
    if (target !== null) {
      targets.push(target)
    }
  }
  return targets
}

/**
 * The room a notification payload names.
 *
 * Android returns the post, `extra` included, under `notification`. iOS
 * returns an id and no `extra` (the plugin keeps `extra` in `userInfo` and
 * does not copy it onto the tap). Desktop 2.5 does not emit a tap at all.
 * A dismiss is not a navigation.
 */
export function notificationClickFromPayload(
  payload: unknown,
  targets: readonly NotificationTarget[],
): NotificationClick | null {
  const record = asRecord(payload)
  if (record === null || record.actionId === 'dismiss') {
    return null
  }
  const direct = clickFromExtra(record.extra)
  if (direct !== null) {
    return direct
  }
  const nested = asRecord(record.notification)
  if (nested !== null) {
    const fromNested = clickFromExtra(nested.extra)
    if (fromNested !== null) {
      return fromNested
    }
  }
  const id =
    numberId(record.id) ?? (nested === null ? null : numberId(nested.id))
  if (id === null) {
    return null
  }
  const target = targets.find((item) => item.id === id)
  if (target === undefined) {
    return null
  }
  return {
    accountId: target.accountId,
    roomId: target.roomId,
    eventId: target.eventId,
    threadRootId: target.threadRootId,
  }
}

/** Map the plugin's permission string onto the states Settings shows. */
export function notificationPermissionState(
  state: string,
): NotificationPermissionState {
  if (state === 'granted') {
    return 'granted'
  }
  if (state === 'denied') {
    return 'denied'
  }
  if (state === 'unsupported') {
    return 'unsupported'
  }
  return 'default'
}

type PermissionListener = (state: NotificationPermissionState) => void

const permissionListeners = new Set<PermissionListener>()

/**
 * Tell every Settings section about a permission result.
 *
 * The badge control and the message control used to keep their own copy, so
 * a grant in one left the other's button on screen. A later request is a
 * no-op once the browser has decided.
 */
export function publishNotificationPermission(
  state: NotificationPermissionState,
): void {
  for (const listener of permissionListeners) {
    listener(state)
  }
}

/** Subscribe to {@link publishNotificationPermission}. */
export function subscribeNotificationPermission(
  listener: PermissionListener,
): () => void {
  permissionListeners.add(listener)
  return () => {
    permissionListeners.delete(listener)
  }
}

function unusedId(targets: readonly NotificationTarget[]): number {
  const used = new Set(targets.map((target) => target.id))
  let id = Date.now() & 0x7fffffff
  if (id === 0) {
    id = 1
  }
  while (used.has(id)) {
    id = (id + 1) & 0x7fffffff
    if (id === 0) {
      id = 1
    }
  }
  return id
}

function asTarget(value: unknown): NotificationTarget | null {
  const record = asRecord(value)
  if (record === null) {
    return null
  }
  const id = numberId(record.id)
  const accountId = record.accountId
  const roomId = record.roomId
  if (
    id === null ||
    typeof accountId !== 'string' ||
    accountId === '' ||
    typeof roomId !== 'string' ||
    roomId === ''
  ) {
    return null
  }
  return {
    id,
    accountId,
    roomId,
    eventId: optionalId(record.eventId),
    threadRootId: optionalId(record.threadRootId),
  }
}

function clickFromExtra(value: unknown): NotificationClick | null {
  const extra = asRecord(value)
  if (extra === null) {
    return null
  }
  const accountId = extra.accountId
  const roomId = extra.roomId
  if (
    typeof accountId !== 'string' ||
    accountId === '' ||
    typeof roomId !== 'string' ||
    roomId === ''
  ) {
    return null
  }
  return {
    accountId,
    roomId,
    eventId: optionalId(extra.eventId),
    threadRootId: optionalId(extra.threadRootId),
  }
}

function optionalId(value: unknown): string | null {
  return typeof value === 'string' && value !== '' ? value : null
}

function asRecord(value: unknown): Record<string, unknown> | null {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    return null
  }
  return value as Record<string, unknown>
}

function numberId(value: unknown): number | null {
  if (typeof value === 'number' && Number.isInteger(value)) {
    return value
  }
  if (typeof value === 'string' && /^-?\d+$/.test(value)) {
    return Number(value)
  }
  return null
}
