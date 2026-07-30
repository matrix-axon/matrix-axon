import { localpart, roomTitle, type RoomDto } from './stores/room-list'

const MATRIX_TO_ORIGIN = 'https://matrix.to'

export interface ResolvedMatrixToRoomLink {
  href: string
  label: string
  isEventLink: boolean
  action: 'open' | 'join'
}

export interface ResolvedMatrixToUserLink {
  href: string
  label: string
  userId: string
}

interface MatrixToTarget {
  mxid: string
  eventId: string | null
  via: string[]
}

export interface MatrixRoomReference {
  roomIdOrAlias: string
  serverNames: string[]
  eventId: string | null
}

export interface MatrixRoomReferenceOptions {
  /**
   * Accept human-friendly alias shorthands (`room:server`, `room@server`,
   * `#room`) for typed commands. External Matrix links stay strict.
   */
  allowAliasShorthand?: boolean
  /** Homeserver name to use when `allowAliasShorthand` receives `#room`. */
  defaultAliasServerName?: string | null
}

export interface MatrixToRoomReferenceLinkOptions {
  via?: readonly string[]
}

export function matrixToLink(
  mxid: string,
  via: readonly string[] = [],
): string {
  let href = `${MATRIX_TO_ORIGIN}/#/${encodeURIComponent(mxid)}`
  const viaParams = via
    .map((server) => server.trim())
    .filter((server) => server !== '')
    .map((server) => `via=${encodeURIComponent(server)}`)
  if (viaParams.length > 0) {
    href += `?${viaParams.join('&')}`
  }
  return href
}

export function matrixToRoomLink(
  room: Pick<RoomDto, 'room_id' | 'canonical_alias' | 'account_user_id'>,
): string {
  return matrixToRoomReferenceLink(room.room_id, room.canonical_alias, {
    via: serverNamesFromUserId(room.account_user_id),
  })
}

export function matrixToRoomReferenceLink(
  roomId: string,
  canonicalAlias: string | null | undefined,
  options: MatrixToRoomReferenceLinkOptions = {},
): string {
  const alias = canonicalAlias?.trim()
  if (alias !== undefined && alias !== '') {
    return matrixToLink(alias)
  }
  const server = serverNameFromRoomId(roomId)
  return matrixToLink(roomId, server === null ? (options.via ?? []) : [server])
}

export function matrixToEventLink(roomId: string, eventId: string): string {
  let href = `${MATRIX_TO_ORIGIN}/#/${encodeURIComponent(roomId)}/${encodeURIComponent(eventId)}`
  const server = serverNameFromRoomId(roomId)
  if (server !== null) {
    href += `?via=${encodeURIComponent(server)}`
  }
  return href
}

export function resolveMatrixToRoomLink(
  href: string,
  context: {
    accountId: string
    rooms: readonly RoomDto[]
    roomTitles: ReadonlyMap<string, string>
  },
  label: string = '',
): ResolvedMatrixToRoomLink | null {
  const target = parseRoomLinkTarget(href)
  if (target === null) {
    return null
  }
  const candidates = context.rooms.filter((room) => {
    if (room.account_id !== context.accountId) {
      return false
    }
    if (target.roomIdOrAlias.startsWith('!')) {
      return room.room_id === target.roomIdOrAlias
    }
    return room.canonical_alias === target.roomIdOrAlias
  })
  const ids = new Set(candidates.map((room) => room.room_id))
  if (ids.size !== 1) {
    return {
      href,
      label: `Join ${target.roomIdOrAlias}`,
      isEventLink: target.eventId !== null,
      action: 'join',
    }
  }
  const room = candidates[0]
  return {
    href: localRoomHref(context.accountId, room.room_id, target.eventId),
    label: linkLabel(label, href, roomTitle(room, context.roomTitles)),
    isEventLink: target.eventId !== null,
    action: 'open',
  }
}

export function parseMatrixRoomReference(
  input: string,
  options: MatrixRoomReferenceOptions = {},
): MatrixRoomReference | null {
  const trimmed = input.trim()
  if (trimmed === '') {
    return null
  }
  const matrixTo = parseMatrixToTarget(trimmed)
  if (matrixTo !== null && isRoomReference(matrixTo.mxid, matrixTo.via)) {
    return {
      roomIdOrAlias: matrixTo.mxid,
      serverNames: matrixTo.via,
      eventId: matrixTo.eventId,
    }
  }
  const matrixUri = parseMatrixUriRoomReference(trimmed)
  if (matrixUri !== null) {
    return matrixUri
  }
  if (trimmed.toLowerCase().startsWith('matrix:')) {
    return null
  }
  if (isRoomReference(trimmed, [])) {
    return {
      roomIdOrAlias: trimmed,
      serverNames: roomReferenceServerNames(trimmed),
      eventId: null,
    }
  }
  if (options.allowAliasShorthand === true) {
    const shorthand = parseAliasShorthand(
      trimmed,
      options.defaultAliasServerName,
    )
    if (shorthand !== null) {
      return {
        roomIdOrAlias: shorthand,
        serverNames: [],
        eventId: null,
      }
    }
  }
  return null
}

export function resolveMatrixToUserLink(
  href: string,
  label: string = '',
  options: { localDm?: boolean } = {},
): ResolvedMatrixToUserLink | null {
  const target = parseMatrixToTarget(href)
  if (target === null || !target.mxid.startsWith('@')) {
    return null
  }
  return {
    href:
      options.localDm === true
        ? localDmHref(target.mxid)
        : matrixToLink(target.mxid),
    label: linkLabel(label, href, localpart(target.mxid)),
    userId: target.mxid,
  }
}

export function localDmHref(userId: string): string {
  return `/rooms/dm?user=${encodeURIComponent(userId)}`
}

function parseMatrixToTarget(href: string): MatrixToTarget | null {
  let url: URL
  try {
    url = new URL(href)
  } catch {
    return null
  }
  if (url.origin !== MATRIX_TO_ORIGIN || !url.hash.startsWith('#/')) {
    return null
  }
  const path = url.hash.slice(2)
  const firstSegment = path.split(/[/?]/, 1)[0]
  if (firstSegment === '') {
    return null
  }
  let mxid: string
  try {
    mxid = decodeURIComponent(firstSegment)
  } catch {
    return null
  }
  if (!mxid.startsWith('!') && !mxid.startsWith('#') && !mxid.startsWith('@')) {
    return null
  }
  const eventId = parseMatrixToEventId(path)
  return { mxid, eventId, via: queryValues(url.hash, 'via') }
}

function isRoomReference(
  roomIdOrAlias: string,
  serverNames: readonly string[],
): boolean {
  if (roomIdOrAlias.startsWith('#')) {
    return serverNameFromRoomReference(roomIdOrAlias) !== null
  }
  if (!roomIdOrAlias.startsWith('!')) {
    return false
  }
  return (
    serverNameFromRoomReference(roomIdOrAlias) !== null ||
    serverNames.length > 0
  )
}

function parseMatrixToEventId(path: string): string | null {
  const pathOnly = path.split('?', 1)[0]
  const secondSegment = pathOnly.split('/')[1]
  if (secondSegment === undefined || secondSegment === '') {
    return null
  }
  let eventId: string
  try {
    eventId = decodeURIComponent(secondSegment)
  } catch {
    return null
  }
  return eventId.startsWith('$') ? eventId : null
}

function parseRoomLinkTarget(href: string): MatrixRoomReference | null {
  const matrixTo = parseMatrixToTarget(href)
  if (matrixTo !== null && isRoomReference(matrixTo.mxid, matrixTo.via)) {
    return {
      roomIdOrAlias: matrixTo.mxid,
      serverNames: matrixTo.via,
      eventId: matrixTo.eventId,
    }
  }
  return parseMatrixUriRoomReference(href)
}

function parseMatrixUriRoomReference(
  input: string,
): MatrixRoomReference | null {
  if (!input.toLowerCase().startsWith('matrix:')) {
    return null
  }
  const withoutScheme = input.slice('matrix:'.length)
  const queryIndex = withoutScheme.indexOf('?')
  const path =
    queryIndex === -1 ? withoutScheme : withoutScheme.slice(0, queryIndex)
  const query = queryIndex === -1 ? '' : withoutScheme.slice(queryIndex + 1)
  const segments = path.split('/').filter((segment) => segment !== '')
  if (segments.length < 2) {
    return null
  }
  const kind = segments[0].toLowerCase()
  let roomIdOrAlias: string
  if (kind === 'r' || kind === 'room') {
    const alias = decodeUriSegment(segments[1])
    if (alias === null || alias === '') {
      return null
    }
    roomIdOrAlias = alias.startsWith('#') ? alias : `#${alias}`
  } else if (kind === 'roomid') {
    const roomId = decodeUriSegment(segments[1])
    if (roomId === null || !roomId.startsWith('!')) {
      return null
    }
    roomIdOrAlias = roomId
  } else {
    return null
  }
  const serverNames = queryValues(query, 'via')
  if (!isRoomReference(roomIdOrAlias, serverNames)) {
    return null
  }
  return {
    roomIdOrAlias,
    serverNames,
    eventId: matrixUriEventId(segments),
  }
}

function matrixUriEventId(segments: string[]): string | null {
  for (let i = 2; i < segments.length - 1; i += 1) {
    if (segments[i].toLowerCase() !== 'e') {
      continue
    }
    const eventId = decodeUriSegment(segments[i + 1])
    return eventId?.startsWith('$') === true ? eventId : null
  }
  return null
}

function queryValues(input: string, name: string): string[] {
  const query = input.includes('?')
    ? input.slice(input.indexOf('?') + 1)
    : input
  const params = new URLSearchParams(query)
  return params
    .getAll(name)
    .map((value) => value.trim())
    .filter((value) => value !== '')
}

function decodeUriSegment(value: string): string | null {
  try {
    return decodeURIComponent(value)
  } catch {
    return null
  }
}

function parseAliasShorthand(
  input: string,
  defaultServerName: string | null | undefined,
): string | null {
  if (/\s/.test(input) || input.includes('://') || /[/?]/.test(input)) {
    return null
  }
  if (
    (!input.startsWith('#') && input.includes('#')) ||
    (input.startsWith('#') && input.indexOf('#', 1) !== -1)
  ) {
    return null
  }
  if (input.startsWith('#')) {
    const localpart = input.slice(1)
    return shorthandAlias(localpart, defaultServerName ?? null)
  }
  if (input.startsWith('!') || input.startsWith('@')) {
    return null
  }
  const atIndex = input.indexOf('@')
  if (atIndex !== -1) {
    if (atIndex === 0 || input.indexOf('@', atIndex + 1) !== -1) {
      return null
    }
    return shorthandAlias(input.slice(0, atIndex), input.slice(atIndex + 1))
  }
  const colonIndex = input.indexOf(':')
  if (colonIndex === -1) {
    return shorthandAlias(input, defaultServerName ?? null)
  }
  return shorthandAlias(input.slice(0, colonIndex), input.slice(colonIndex + 1))
}

function shorthandAlias(
  localpart: string,
  serverName: string | null,
): string | null {
  if (
    localpart === '' ||
    localpart.includes(':') ||
    localpart.includes('@') ||
    localpart.includes('/') ||
    serverName === null ||
    serverName === '' ||
    serverName.includes('@') ||
    serverName.includes('/')
  ) {
    return null
  }
  return `#${localpart}:${serverName}`
}

export function localRoomHref(
  accountId: string,
  roomId: string,
  eventId: string | null,
): string {
  const roomHref = `/${encodeURIComponent(accountId)}/rooms/${encodeURIComponent(roomId)}`
  return eventId === null
    ? roomHref
    : `${roomHref}?event=${encodeURIComponent(eventId)}`
}

/** A local deep link that keeps a thread open while revealing one of its replies. */
export function localThreadEventHref(
  accountId: string,
  roomId: string,
  threadRootId: string,
  eventId: string,
): string {
  const roomHref = `/${encodeURIComponent(accountId)}/rooms/${encodeURIComponent(roomId)}`
  return `${roomHref}?thread=${encodeURIComponent(threadRootId)}&event=${encodeURIComponent(eventId)}`
}

function linkLabel(label: string, href: string, roomName: string): string {
  const trimmed = label.trim()
  return trimmed === '' || trimmed === href ? roomName : label
}

function serverNameFromRoomId(roomId: string): string | null {
  return serverNameFromRoomReference(roomId)
}

export function serverNameFromRoomReference(
  roomIdOrAlias: string,
): string | null {
  const colon = roomIdOrAlias.indexOf(':')
  if (colon === -1 || colon === roomIdOrAlias.length - 1) {
    return null
  }
  return roomIdOrAlias.slice(colon + 1)
}

function roomReferenceServerNames(roomIdOrAlias: string): string[] {
  if (!roomIdOrAlias.startsWith('!')) {
    return []
  }
  const server = serverNameFromRoomId(roomIdOrAlias)
  return server === null ? [] : [server]
}

export function serverNamesFromUserId(
  userId: string | null | undefined,
): string[] {
  const server =
    userId === null || userId === undefined
      ? null
      : serverNameFromRoomReference(userId)
  return server === null ? [] : [server]
}
