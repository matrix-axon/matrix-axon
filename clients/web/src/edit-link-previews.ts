import { matchUrls } from './html/linkify'
import type { PreviewInput } from './incoming-share/contract'
import { parseLinkPreviews, type LinkPreview } from './link-previews'
import {
  wirePreview,
  type OutgoingLinkPreview,
  type PreviewDraft,
} from './link-preview-send'
import { linkIdentity, nativePreviewLinks } from './preview-links'

/**
 * Link previews on an edit (ADR 0132 § 8): which of a message's cards edit
 * mode shows, and what the edit sends, decided against the exact replacement
 * the server will build. The server carries a message's previews forward when
 * an edit leaves the field out, so an edit sends the field only when it
 * changes them.
 */

/** The server's limits (`crates/axon-core/src/link_preview.rs`). */
const MAX_ENTRIES = 8
const MAX_PREVIEW_BYTES = 32 * 1024
const MAX_CONTENT_BYTES = 40 * 1024
/** The server's reserve for an image descriptor not yet uploaded. */
const IMAGE_RESERVE = 'x'.repeat(1280)
const PLACEHOLDER_UPLOAD_ID = '00000000-0000-4000-8000-000000000001'

/** The keys the server writes in an entry, and nothing else. */
const ENTRY_KEYS: ReadonlySet<string> = new Set([
  'matrix:matched_url',
  'og:title',
  'og:description',
  'og:site_name',
  'og:image',
  'matrix:image:encrypted',
  'og:image:type',
  'og:image:width',
  'og:image:height',
  'matrix:image:size',
])

/** The keys of the encrypted-file object the server writes. */
const ENCRYPTED_FILE_KEYS: ReadonlySet<string> = new Set([
  'url',
  'key',
  'iv',
  'hashes',
  'v',
])
const IMAGE_TYPES: ReadonlySet<unknown> = new Set([
  'image/png',
  'image/jpeg',
  'image/webp',
])
const MAX_URL_UNITS = 4096
const MAX_MXC_BYTES = 1024
const MAX_MEDIA_SOURCE_BYTES = 1024
const MAX_IMAGE_BYTES = 2 * 1024 * 1024
const MAX_IMAGE_PIXELS = 4_000_000

const bytes = (value: unknown) =>
  new TextEncoder().encode(JSON.stringify(value)).length

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null
}

/** A well-formed `mxc://server/media` reference, as the server parses one. */
function mxcIsValid(value: unknown): boolean {
  if (typeof value !== 'string' || !value.startsWith('mxc://')) return false
  if (new TextEncoder().encode(value).length > MAX_MXC_BYTES) return false
  const rest = value.slice('mxc://'.length)
  const slash = rest.indexOf('/')
  if (slash === -1) return false
  const media = rest.slice(slash + 1)
  return slash > 0 && media !== '' && !media.includes('/')
}

function encryptedFileIsValid(value: unknown): boolean {
  const file = record(value)
  return (
    file !== null &&
    Object.keys(file).every((key) => ENCRYPTED_FILE_KEYS.has(key)) &&
    mxcIsValid(file.url) &&
    record(file.key) !== null &&
    typeof file.iv === 'string' &&
    typeof record(file.hashes)?.sha256 === 'string' &&
    typeof file.v === 'string' &&
    bytes(file) <= MAX_MEDIA_SOURCE_BYTES
  )
}

/** A positive whole number no larger than `max`, when present. */
function countWithin(value: unknown, max: number): boolean {
  return (
    value === undefined ||
    (typeof value === 'number' &&
      Number.isSafeInteger(value) &&
      value > 0 &&
      value <= max)
  )
}

/** Text no longer than `limit` UTF-16 units, when present. */
function textWithin(value: unknown, limit: number): boolean {
  return (
    value === undefined || (typeof value === 'string' && value.length <= limit)
  )
}

/**
 * Whether the server would use this current entry, in the standard
 * spelling: the checks a new entry gets (`entry_is_valid` in
 * `crates/axon-core/src/edit_preview.rs`). The timeline's renderer is more
 * forgiving, truncating text and dropping a bad image, so a card it shows can
 * still be one the server leaves out of an edit.
 */
function entryIsValid(entry: Record<string, unknown>): boolean {
  if (Object.keys(entry).some((key) => !ENTRY_KEYS.has(key))) return false
  const matched = entry['matrix:matched_url']
  if (typeof matched !== 'string' || matched.length > MAX_URL_UNITS)
    return false
  let url: URL
  try {
    url = new URL(matched)
  } catch {
    return false
  }
  if (
    !['http:', 'https:'].includes(url.protocol) ||
    url.username !== '' ||
    url.password !== ''
  )
    return false
  const plain = Object.hasOwn(entry, 'og:image')
  const encrypted = Object.hasOwn(entry, 'matrix:image:encrypted')
  if (plain && encrypted) return false
  if (plain && !mxcIsValid(entry['og:image'])) return false
  if (encrypted && !encryptedFileIsValid(entry['matrix:image:encrypted']))
    return false
  const width = entry['og:image:width']
  const height = entry['og:image:height']
  return (
    textWithin(entry['og:title'], 300) &&
    textWithin(entry['og:description'], 1000) &&
    textWithin(entry['og:site_name'], 120) &&
    (entry['og:image:type'] === undefined ||
      IMAGE_TYPES.has(entry['og:image:type'])) &&
    countWithin(width, MAX_IMAGE_PIXELS) &&
    countWithin(height, MAX_IMAGE_PIXELS) &&
    (typeof width !== 'number' ||
      typeof height !== 'number' ||
      width * height <= MAX_IMAGE_PIXELS) &&
    countWithin(entry['matrix:image:size'], MAX_IMAGE_BYTES)
  )
}

/** A message's preview field, as the server reads it for an edit. */
export interface MessagePreviewField {
  /** The message has a preview field, even an empty one. */
  hasField: boolean
  /** The entries the server would use, in the standard spelling, by link
   *  identity: among the first eight, those passing its checks, the first
   *  for each link. */
  entries: ReadonlyMap<string, Record<string, unknown>>
}

/**
 * Read `content`'s preview field: `m.url_previews` when present, even when it
 * is empty or not a list; otherwise `com.beeper.linkpreviews`, converted.
 */
export function messagePreviewField(content: unknown): MessagePreviewField {
  const c = record(content)
  const entries = new Map<string, Record<string, unknown>>()
  if (c === null) return { hasField: false, entries }
  const standard = Object.hasOwn(c, 'm.url_previews')
  if (!standard && !Object.hasOwn(c, 'com.beeper.linkpreviews'))
    return { hasField: false, entries }
  const raw = standard ? c['m.url_previews'] : c['com.beeper.linkpreviews']
  if (!Array.isArray(raw)) return { hasField: true, entries }
  raw.slice(0, MAX_ENTRIES).forEach((value) => {
    const source = record(value)
    if (source === null) return
    const entry: Record<string, unknown> = { ...source }
    if (!standard) {
      for (const [alias, key] of [
        ['matched_url', 'matrix:matched_url'],
        ['beeper:image:encryption', 'matrix:image:encrypted'],
      ] as const)
        if (Object.hasOwn(entry, alias)) {
          entry[key] = entry[alias]
          delete entry[alias]
        }
    }
    if (!entryIsValid(entry)) return
    const identity = linkIdentity(entry['matrix:matched_url'] as string)
    if (identity !== null && !entries.has(identity))
      entries.set(identity, entry)
  })
  return { hasField: true, entries }
}

/** What edit mode starts from (ADR 0132 § 8). */
export interface EditPreviewSeed {
  /** The message's body as edit mode opened: its links are never added as
   *  new cards, so opening edit mode fetches nothing. */
  originalText: string
  /** The message's cards, as the timeline renders them and the server would
   *  keep them, in its order. */
  cards: readonly LinkPreview[]
  /** Whether the edit may add new cards: not for a media caption (§ 8). */
  addNew: boolean
  /** Links discarded earlier in this edit, by identity, carried across a
   *  reopen after a failed save. */
  discarded?: ReadonlySet<string>
  /** Links whose new cards the server refused, by identity: shown with no
   *  card for the rest of the edit. */
  refused?: ReadonlySet<string>
}

/** The cards edit mode shows for `event`: those the timeline renders that
 *  the server would also keep. A message with a spoiler shows none. */
export function messageCards(
  content: unknown,
  body: string | null | undefined,
): LinkPreview[] {
  const { entries } = messagePreviewField(content)
  return parseLinkPreviews(content, body).filter((card) =>
    entries.has(card.url),
  )
}

/** One entry of an edit's explicit list, in text order. */
export type EditLinkPreview =
  | { kind: 'keep'; matchedUrl: string }
  | { kind: 'new'; preview: OutgoingLinkPreview }

export type EditPreviewPlan =
  | { kind: 'omit' }
  | { kind: 'send'; previews: EditLinkPreview[] }
  /** Even `[]` does not fit, and leaving the field out would lose the
   *  message's marker or expose a discarded link. The user decides. */
  | { kind: 'too-long-for-marker' }

/** What an edit is about to send, and what it replaces. */
export interface EditRequest {
  eventId: string
  body: string
  formattedBody: string | null
  /** The message's content as it stands. */
  content: unknown
}

/** The cards an edit's composer hands its save. */
export interface EditPreviewDraft extends PreviewDraft {
  /** The message's cards still shown, by their link's text in the body. */
  kept: readonly string[]
  /** The message's cards as edit mode opened, by identity. */
  opened: ReadonlySet<string>
}

/** The msgtypes whose body is a caption: an edit keeps the media's fields,
 *  and adds no cards (§ 8). */
export const MEDIA_MSGTYPES: ReadonlySet<unknown> = new Set([
  'm.image',
  'm.file',
  'm.audio',
  'm.video',
])

/**
 * The replacement the server builds, close enough to measure: the new text
 * twice, once in `m.new_content` with `entries` and once in the `* ` fallback
 * with `[]` of both spellings. A media caption keeps the media's fields.
 */
function replacement(
  request: EditRequest,
  entries: readonly Record<string, unknown>[],
): { content: Record<string, unknown>; previews: unknown } {
  const original = record(request.content) ?? {}
  const msgtype =
    typeof original.msgtype === 'string' ? original.msgtype : 'm.text'
  const base: Record<string, unknown> = { msgtype }
  if (MEDIA_MSGTYPES.has(msgtype)) {
    for (const key of ['url', 'file', 'info'])
      if (Object.hasOwn(original, key)) base[key] = original[key]
    base.filename = original.filename ?? original.body ?? ''
  }
  const text = (prefix: string) => ({
    ...base,
    body: prefix + request.body,
    ...(request.formattedBody !== null && {
      format: 'org.matrix.custom.html',
      formatted_body: prefix + request.formattedBody,
    }),
  })
  const compat = entries.map((entry) => {
    const result: Record<string, unknown> = { ...entry }
    result.matched_url = result['matrix:matched_url']
    delete result['matrix:matched_url']
    if (result['matrix:image:encrypted'] !== undefined) {
      result['beeper:image:encryption'] = result['matrix:image:encrypted']
      delete result['matrix:image:encrypted']
    }
    return result
  })
  return {
    previews: entries,
    content: {
      ...text('* '),
      'm.new_content': {
        ...text(''),
        'm.url_previews': entries,
        'com.beeper.linkpreviews': compat,
      },
      'm.relates_to': { rel_type: 'm.replace', event_id: request.eventId },
      'm.url_previews': [],
      'com.beeper.linkpreviews': [],
    },
  }
}

/** Whether the replacement carrying `previews` fits the server's budgets. */
export function editFits(
  request: EditRequest,
  previews: readonly EditLinkPreview[],
  field: MessagePreviewField = messagePreviewField(request.content),
): boolean {
  const entries = previews.map((preview): Record<string, unknown> => {
    if (preview.kind === 'keep') {
      const identity = linkIdentity(preview.matchedUrl)
      const current =
        identity === null ? undefined : field.entries.get(identity)
      return { ...current, 'matrix:matched_url': preview.matchedUrl }
    }
    const wire = wirePreview(
      preview.preview,
      preview.preview.image === null ? null : PLACEHOLDER_UPLOAD_ID,
    )
    const entry: Record<string, unknown> = {
      'matrix:matched_url': wire.matched_url,
    }
    if (wire.title != null) entry['og:title'] = wire.title
    if (wire.description != null) entry['og:description'] = wire.description
    if (wire.site_name != null) entry['og:site_name'] = wire.site_name
    if (wire.image_upload_id != null)
      entry['matrix:image:encrypted'] = IMAGE_RESERVE
    return entry
  })
  const built = replacement(request, entries)
  return (
    bytes(built.previews) <= MAX_PREVIEW_BYTES &&
    bytes(built.content) <= MAX_CONTENT_BYTES
  )
}

/**
 * Decide an edit's `url_previews` (ADR 0132 § 8). A message card counts as
 * removed when it was shown as edit mode opened and is not sent: the user
 * discarded it, its link left the text, or the size check dropped it. The
 * first matching rule wins:
 *
 * 1. a spoiler: `[]` if the message has a preview field, otherwise omit;
 * 2. every card the message showed kept, none new, and all of them fit:
 *    omit, and the server carries them forward against the message as it
 *    now is, so a plain text edit never meets `409 previews_changed`;
 * 3. a card kept or ready after the size check: those cards, in text order,
 *    the message's as `keep` entries;
 * 4. a message card removed, or, with previews on, an eligible link added:
 *    `[]`, or the marker question when even that does not fit;
 * 5. otherwise omit, and the server carries the previews forward.
 *
 * The size check drops new cards' images, last first, then new cards, then
 * the message's cards, which are copied whole.
 */
export function planEditPreviews(
  request: EditRequest,
  draft: EditPreviewDraft,
  {
    originalText,
    previewsOn,
    allowUnmarked = false,
  }: { originalText: string; previewsOn: boolean; allowUnmarked?: boolean },
): EditPreviewPlan {
  const field = messagePreviewField(request.content)
  if (/data-mx-spoiler/i.test(request.formattedBody ?? ''))
    return field.hasField ? { kind: 'send', previews: [] } : { kind: 'omit' }
  const eligible = nativePreviewLinks(request.body)
  const bare = matchUrls(request.body).map(({ url }) => url)
  const position = (url: string) => bare.indexOf(url)
  const counted = new Set<string>()
  // A kept card needs only its link in the body and an entry the server will
  // copy: the fetch policy in `eligible` governs new cards, and the server
  // keeps a card for any link it accepted (§ 8).
  const take = (url: string, fetched: boolean) => {
    const identity = linkIdentity(url)
    if (
      identity === null ||
      counted.has(identity) ||
      position(url) === -1 ||
      (fetched ? !eligible.includes(url) : !field.entries.has(identity))
    )
      return false
    counted.add(identity)
    return true
  }
  const kept = draft.kept.filter((url) => take(url, false))
  const fresh = draft.cards.filter((card) => take(card.matchedUrl, true))
  const fits = (list: readonly EditLinkPreview[]) =>
    editFits(request, list, field)
  // Kept cards are distinct and among those opened, so equal counts mean
  // every one is kept. When they do not all fit, the rules below degrade
  // them, or ask, rather than leave it to the server to drop the field.
  if (
    fresh.length === 0 &&
    kept.length > 0 &&
    kept.length === draft.opened.size &&
    fits(kept.map((matchedUrl) => ({ kind: 'keep', matchedUrl })))
  )
    return { kind: 'omit' }
  let previews: EditLinkPreview[] = [
    ...kept.map((matchedUrl) => ({ kind: 'keep' as const, matchedUrl })),
    ...fresh.map((preview) => ({ kind: 'new' as const, preview })),
  ].sort(
    (a, b) =>
      position(a.kind === 'keep' ? a.matchedUrl : a.preview.matchedUrl) -
      position(b.kind === 'keep' ? b.matchedUrl : b.preview.matchedUrl),
  )
  while (previews.length > 0 && !fits(previews)) {
    const image = previews.findLastIndex(
      (p) => p.kind === 'new' && p.preview.image !== null,
    )
    if (image !== -1) {
      previews = previews.map((p, index) =>
        index === image && p.kind === 'new'
          ? { kind: 'new', preview: { ...p.preview, image: null } }
          : p,
      )
      continue
    }
    const fresh = previews.findLastIndex((p) => p.kind === 'new')
    const drop = fresh !== -1 ? fresh : previews.length - 1
    previews = previews.filter((_, index) => index !== drop)
  }
  if (previews.length > 0) return { kind: 'send', previews }
  // Nothing is sent, so every card the message showed is removed.
  const removed = draft.opened.size > 0
  const original = new Set(
    nativePreviewLinks(originalText).map((url) => linkIdentity(url)),
  )
  const added = eligible.some((url) => !original.has(linkIdentity(url)))
  if (!removed && !(previewsOn && added)) return { kind: 'omit' }
  if (fits([])) return { kind: 'send', previews: [] }
  const discardedInBody = eligible.some((url) => {
    const identity = linkIdentity(url)
    return identity !== null && draft.discarded.has(identity)
  })
  return (field.hasField || discardedInBody) && !allowUnmarked
    ? { kind: 'too-long-for-marker' }
    : { kind: 'omit' }
}

/** The wire list for an edit, with each new card's upload id. */
export function wireEditPreviews(
  previews: readonly EditLinkPreview[],
  uploadIds: readonly (string | null)[],
): (PreviewInput | { matched_url: string; keep: true })[] {
  let next = 0
  return previews.map((preview) =>
    preview.kind === 'keep'
      ? { matched_url: preview.matchedUrl, keep: true as const }
      : wirePreview(preview.preview, uploadIds[next++] ?? null),
  )
}
