import { sanitizeOutgoingHtml } from './html/sanitize'
import {
  matrixToLink,
  matrixToRoomReferenceLink,
  serverNamesFromUserId,
} from './matrix-to'
import { markdownToHtmlIfFormatted } from './markdown/markdown'
import { replaceEmojiShortcodes, type EmojiEntry } from './emoji'
import { protectInlineEmoji, type InlineEmojiReference } from './inline-emoji'
import type { MemberDto, RoomDto } from './stores/room-list'
import {
  localpart,
  memberDisplay,
  roomKey,
  roomTitle,
} from './stores/room-list'

export interface MentionSuggestion {
  value: string
  label: string
  description: string
}

export interface MessageLinkContext {
  accountId: string
  members: readonly MemberDto[]
  rooms: readonly RoomDto[]
  roomTitles: ReadonlyMap<string, string>
  emojiEntries?: readonly EmojiEntry[]
  inlineEmoji?: readonly InlineEmojiReference[]
}

export interface FormattedMessageParts {
  body: string
  format?: 'org.matrix.custom.html'
  formatted_body?: string
}

interface UserCandidate {
  userId: string
  token: string
  display: string
}

interface RoomCandidate {
  accountId: string
  accountUserId: string
  roomId: string
  canonicalAlias: string | null
  token: string
  title: string
}

interface LinkMatch {
  start: number
  end: number
  html: string
}

const BOUNDARY_BEFORE = /[\s([{'"“‘]/
const BOUNDARY_AFTER = /[\s.,!?;:)\]}'"”’]/

export function userMentionSuggestions(
  members: readonly MemberDto[],
  query: string,
): MentionSuggestion[] {
  const normalized = query.toLowerCase()
  return userCandidates(members)
    .filter((candidate) =>
      [
        candidate.token,
        candidate.display,
        candidate.userId,
        candidate.userId.slice(1),
      ].some((field) => field.toLowerCase().startsWith(normalized)),
    )
    .slice(0, 8)
    .map((candidate) => ({
      value: candidate.token,
      label: candidate.token,
      description:
        candidate.display === candidate.token
          ? candidate.userId
          : `${candidate.display} · ${candidate.userId}`,
    }))
}

export function roomReferenceSuggestions(
  rooms: readonly RoomDto[],
  roomTitles: ReadonlyMap<string, string>,
  query: string,
): MentionSuggestion[] {
  const normalized = query.toLowerCase()
  return roomCandidates(rooms, roomTitles)
    .filter((candidate) =>
      roomSearchFields(candidate).some((field) =>
        field.toLowerCase().startsWith(normalized),
      ),
    )
    .slice(0, 8)
    .map((candidate) => ({
      value: candidate.token,
      label: candidate.token,
      description:
        candidate.title === candidate.token
          ? candidate.roomId
          : candidate.title,
    }))
}

export function formatMessageBody(
  body: string,
  context: MessageLinkContext,
): FormattedMessageParts {
  const inline = protectInlineEmoji(body, context.inlineEmoji)
  const expandedBody = replaceEmojiShortcodes(inline.text, context.emojiEntries)
  const markdownHtml = markdownToHtmlIfFormatted(expandedBody)
  if (markdownHtml !== null) {
    const linked = linkTextNodes(markdownHtml, context)
    return {
      body: inline.plain(expandedBody),
      format: 'org.matrix.custom.html',
      formatted_body: sanitizeOutgoingHtml(inline.render(linked.html).html, {
        inlineEmoji: true,
      }),
    }
  }

  const linked = linkPlainText(expandedBody, context)
  const rendered = inline.render(linked.html)
  if (!linked.changed && !rendered.changed) {
    return { body: inline.plain(expandedBody) }
  }
  return {
    body: inline.plain(expandedBody),
    format: 'org.matrix.custom.html',
    formatted_body: sanitizeOutgoingHtml(rendered.html, { inlineEmoji: true }),
  }
}

function userCandidates(members: readonly MemberDto[]): UserCandidate[] {
  const localCounts = new Map<string, number>()
  for (const member of members) {
    if (member.membership !== 'join') {
      continue
    }
    const token = localpart(member.user_id).toLowerCase()
    localCounts.set(token, (localCounts.get(token) ?? 0) + 1)
  }

  return members
    .filter((member) => member.membership === 'join')
    .map((member) => {
      const local = localpart(member.user_id)
      return {
        userId: member.user_id,
        token:
          (localCounts.get(local.toLowerCase()) ?? 0) > 1
            ? member.user_id
            : local,
        display: memberDisplay(member),
      }
    })
    .sort((a, b) => a.token.localeCompare(b.token))
}

function roomCandidates(
  rooms: readonly RoomDto[],
  roomTitles: ReadonlyMap<string, string>,
): RoomCandidate[] {
  const result: RoomCandidate[] = []
  const seen = new Set<string>()
  for (const room of rooms) {
    if (room.room_id === null || room.room_id === undefined) {
      continue
    }
    const key = roomKey(room)
    if (seen.has(key)) {
      continue
    }
    seen.add(key)
    const title = roomTitle(room, roomTitles)
    const token = room.canonical_alias ?? titleToRoomToken(title)
    if (token.trim() === '#') {
      continue
    }
    result.push({
      accountId: room.account_id,
      accountUserId: room.account_user_id,
      roomId: room.room_id,
      canonicalAlias: room.canonical_alias ?? null,
      token,
      title,
    })
  }
  return result
}

function titleToRoomToken(title: string): string {
  return title.startsWith('#') || title.startsWith('!') ? title : `#${title}`
}

function roomSearchFields(candidate: RoomCandidate): string[] {
  const fields = [candidate.token, candidate.title, candidate.roomId]
  const local = candidate.token.startsWith('#')
    ? candidate.token.slice(1).split(':', 1)[0]
    : null
  if (local !== null && local !== '') {
    fields.push(local)
  }
  return fields
}

function linkTextNodes(
  html: string,
  context: MessageLinkContext,
): { html: string; changed: boolean } {
  const scratch = document.createElement('div')
  scratch.innerHTML = html
  let changed = false
  const walker = document.createTreeWalker(scratch, NodeFilter.SHOW_TEXT)
  const textNodes: Text[] = []
  while (walker.nextNode()) {
    textNodes.push(walker.currentNode as Text)
  }
  for (const node of textNodes) {
    const parent = node.parentElement
    if (
      parent?.closest('a, code, pre') !== null &&
      parent?.closest('a, code, pre') !== undefined
    ) {
      continue
    }
    const linked = linkPlainText(node.data, context)
    if (!linked.changed) {
      continue
    }
    const template = document.createElement('template')
    template.innerHTML = linked.html
    node.replaceWith(template.content)
    changed = true
  }
  return { html: scratch.innerHTML, changed }
}

function linkPlainText(
  text: string,
  context: MessageLinkContext,
): { html: string; changed: boolean } {
  const matches = findLinkMatches(text, context)
  if (matches.length === 0) {
    return { html: escapeText(text), changed: false }
  }
  let html = ''
  let last = 0
  for (const match of matches) {
    html += escapeText(text.slice(last, match.start))
    html += match.html
    last = match.end
  }
  html += escapeText(text.slice(last))
  return { html, changed: true }
}

function findLinkMatches(
  text: string,
  context: MessageLinkContext,
): LinkMatch[] {
  const users = userCandidates(context.members)
  const rooms = roomCandidates(
    context.rooms.filter((room) => room.account_id === context.accountId),
    context.roomTitles,
  ).sort((a, b) => b.token.length - a.token.length)
  const matches: LinkMatch[] = []
  let index = 0
  while (index < text.length) {
    const char = text[index]
    if (char !== '@' && char !== '#') {
      index += 1
      continue
    }
    if (!hasBoundaryBefore(text, index)) {
      index += 1
      continue
    }
    const match =
      char === '@'
        ? matchUser(text, index, users)
        : matchRoom(text, index, rooms)
    if (match === null) {
      index += 1
      continue
    }
    matches.push(match)
    index = match.end
  }
  return matches
}

function matchUser(
  text: string,
  start: number,
  users: readonly UserCandidate[],
): LinkMatch | null {
  const token = /^@[^\s<>()\]{}"'[]+/.exec(text.slice(start))?.[0]
  if (token === undefined || token === '@') {
    return null
  }
  const trimmed = trimTrailingPunctuation(token)
  const end = start + trimmed.length
  if (!hasBoundaryAfter(text, end)) {
    return null
  }
  const normalized = trimmed.toLowerCase()
  const matches = users.filter((candidate) =>
    userMatchKeys(candidate).some((key) => key.toLowerCase() === normalized),
  )
  const unique = uniqueUser(matches)
  if (unique === null) {
    return null
  }
  return {
    start,
    end,
    html: `<a class="mention-pill" href="${escapeAttr(matrixToLink(unique.userId))}">${escapeText(trimmed)}</a>`,
  }
}

function matchRoom(
  text: string,
  start: number,
  rooms: readonly RoomCandidate[],
): LinkMatch | null {
  for (const room of rooms) {
    for (const token of roomMatchTokens(room)) {
      if (!startsWithIgnoreCase(text, start, token)) {
        continue
      }
      const end = start + token.length
      if (!hasBoundaryAfter(text, end)) {
        continue
      }
      const matchingRooms = rooms.filter((candidate) =>
        roomMatchTokens(candidate).some((candidateToken) =>
          equalsIgnoreCase(candidateToken, token),
        ),
      )
      const unique = uniqueRoom(matchingRooms)
      if (unique === null) {
        return null
      }
      return {
        start,
        end,
        html: `<a class="room-pill" href="${escapeAttr(matrixToRoomReferenceLink(unique.roomId, unique.canonicalAlias, { via: serverNamesFromUserId(unique.accountUserId) }))}">${escapeText(text.slice(start, end))}</a>`,
      }
    }
  }
  return null
}

function userMatchKeys(candidate: UserCandidate): string[] {
  const keys = [candidate.token, candidate.userId]
  const display = candidate.display.trim()
  if (display !== '' && !/\s/.test(display)) {
    keys.push(display.startsWith('@') ? display : `@${display}`)
  }
  return keys
}

function roomMatchTokens(candidate: RoomCandidate): string[] {
  const tokens = [candidate.token]
  if (candidate.title.trim() !== '') {
    tokens.push(titleToRoomToken(candidate.title))
  }
  const local = candidate.token.startsWith('#')
    ? candidate.token.slice(1).split(':', 1)[0]
    : null
  if (local !== null && local !== '') {
    tokens.push(`#${local}`)
  }
  return [...new Set(tokens)]
}

function uniqueUser(
  candidates: readonly UserCandidate[],
): UserCandidate | null {
  const ids = new Set(candidates.map((candidate) => candidate.userId))
  return ids.size === 1 ? candidates[0] : null
}

function uniqueRoom(
  candidates: readonly RoomCandidate[],
): RoomCandidate | null {
  const ids = new Set(
    candidates.map(
      (candidate) => `${candidate.accountId}\0${candidate.roomId}`,
    ),
  )
  return ids.size === 1 ? candidates[0] : null
}

function hasBoundaryBefore(text: string, index: number): boolean {
  return index === 0 || BOUNDARY_BEFORE.test(text[index - 1])
}

function hasBoundaryAfter(text: string, index: number): boolean {
  return index >= text.length || BOUNDARY_AFTER.test(text[index])
}

function startsWithIgnoreCase(
  text: string,
  index: number,
  token: string,
): boolean {
  return (
    text.slice(index, index + token.length).toLowerCase() ===
    token.toLowerCase()
  )
}

function equalsIgnoreCase(left: string, right: string): boolean {
  return left.toLowerCase() === right.toLowerCase()
}

function trimTrailingPunctuation(token: string): string {
  return token.replace(/[.,!?;]+$/, '')
}

function escapeText(text: string): string {
  return text
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/\n/g, '<br>')
}

function escapeAttr(text: string): string {
  return escapeText(text).replace(/"/g, '&quot;')
}
