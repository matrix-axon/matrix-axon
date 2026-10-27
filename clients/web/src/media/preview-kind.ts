import { MAX_UPLOAD_BYTES } from './media-service'
import type { ParsedMedia } from './parse-media'

/**
 * How a non-image attachment can be previewed on demand (ADR 0072), or `null`
 * when it stays a plain download card.
 */
export type PreviewKind = 'audio' | 'video' | 'pdf' | 'text'

/**
 * Largest object each kind will load for preview, in bytes. The preview is
 * blob-backed — the whole object lands in memory before playback starts (there
 * is no `Range` streaming until the media service worker of ADR 0072's
 * follow-up) — so these are memory ceilings, not policy. Above them the card
 * keeps its Download-only behaviour.
 *
 * Binary kinds sit at `MAX_UPLOAD_BYTES`, the largest object our own clients
 * can put in a room: a lower bar would have made a phone video — the single
 * most likely thing to want to play inline — the one thing that could not be
 * played, which is the wrong trade for saving memory a browser has. Text is
 * capped far lower because a `<pre>` of a megabyte is unreadable, not because
 * of memory.
 */
const SIZE_CEILING: Record<PreviewKind, number> = {
  audio: MAX_UPLOAD_BYTES,
  video: MAX_UPLOAD_BYTES,
  pdf: MAX_UPLOAD_BYTES,
  text: 512 * 1024,
}

/**
 * Extension → `(kind, MIME)`, for events whose `info.mimetype` is missing or
 * generic. This is not a nicety: an iPhone video sent through the web client
 * arrives as `msgtype: m.file` with `mimetype: application/octet-stream` and
 * `filename: IMG_9306.mov`, so the filename is the *only* signal that it is a
 * video at all. axon-tui makes the same fallback on the send side
 * (`media_kind_and_content_type`, which maps an extension because a path on
 * disk is all it has).
 *
 * The MIME here is also what the blob gets re-typed to, because a `<video>`
 * will not play an `application/octet-stream` blob — so a wrong entry means a
 * dead player, and every value must stay inert. Nothing maps to `text/html` or
 * `image/svg+xml`, which is what keeps the re-type from becoming a way to run
 * attacker markup in our origin.
 */
const EXTENSION_TYPE: Record<string, { kind: PreviewKind; mime: string }> = {
  // audio
  m4a: { kind: 'audio', mime: 'audio/mp4' },
  mp3: { kind: 'audio', mime: 'audio/mpeg' },
  oga: { kind: 'audio', mime: 'audio/ogg' },
  ogg: { kind: 'audio', mime: 'audio/ogg' },
  opus: { kind: 'audio', mime: 'audio/ogg' },
  wav: { kind: 'audio', mime: 'audio/wav' },
  flac: { kind: 'audio', mime: 'audio/flac' },
  aac: { kind: 'audio', mime: 'audio/aac' },
  // video
  mov: { kind: 'video', mime: 'video/quicktime' },
  mp4: { kind: 'video', mime: 'video/mp4' },
  m4v: { kind: 'video', mime: 'video/mp4' },
  webm: { kind: 'video', mime: 'video/webm' },
  mkv: { kind: 'video', mime: 'video/x-matroska' },
  // documents
  pdf: { kind: 'pdf', mime: 'application/pdf' },
  // text
  txt: { kind: 'text', mime: 'text/plain' },
  md: { kind: 'text', mime: 'text/plain' },
  log: { kind: 'text', mime: 'text/plain' },
  json: { kind: 'text', mime: 'text/plain' },
  csv: { kind: 'text', mime: 'text/plain' },
  yaml: { kind: 'text', mime: 'text/plain' },
  yml: { kind: 'text', mime: 'text/plain' },
  toml: { kind: 'text', mime: 'text/plain' },
  ini: { kind: 'text', mime: 'text/plain' },
  rs: { kind: 'text', mime: 'text/plain' },
  ts: { kind: 'text', mime: 'text/plain' },
  tsx: { kind: 'text', mime: 'text/plain' },
  js: { kind: 'text', mime: 'text/plain' },
  py: { kind: 'text', mime: 'text/plain' },
  sh: { kind: 'text', mime: 'text/plain' },
}

/** Text-ish `application/*` types the sender may declare. */
const TEXT_TYPES = new Set([
  'application/json',
  'application/xml',
  'application/x-yaml',
  'application/yaml',
])

/** What an inline preview resolved an attachment to. */
export interface PreviewPlan {
  kind: PreviewKind
  /**
   * The MIME to re-type the blob to before it reaches the DOM, or `null` to
   * keep what the proxy sent. Non-null whenever the served type would not
   * render: the proxy downgrades anything that is not image/audio/video to
   * `application/octet-stream` (`is_inline_safe`, `routes/media.rs`), and a
   * sender-declared `application/octet-stream` comes back unchanged — neither
   * plays in a media element or renders in a PDF viewer.
   */
  contentType: string | null
}

/** The bare `type/subtype`, lowercased. */
function normalizeMime(mimetype: string): string {
  return mimetype.split(';')[0]!.trim().toLowerCase()
}

/**
 * Whether a declared mimetype carries no information — the generic binary
 * types a sender uses when it could not determine one. These fall through to
 * the msgtype/extension fallbacks; any *other* unrecognised type is a positive
 * statement about the bytes and refuses the preview outright.
 */
function isUnhelpfulMimetype(mime: string): boolean {
  return (
    mime === 'application/octet-stream' ||
    mime === 'binary/octet-stream' ||
    mime === 'application/unknown'
  )
}

function kindFromMimetype(mime: string): PreviewKind | null {
  if (mime === 'application/pdf') {
    return 'pdf'
  }
  if (mime === 'text/html' || mime === 'image/svg+xml') {
    return null
  }
  if (mime.startsWith('text/') || TEXT_TYPES.has(mime)) {
    return 'text'
  }
  if (mime.startsWith('audio/')) {
    return 'audio'
  }
  if (mime.startsWith('video/')) {
    return 'video'
  }
  return null
}

function fromFilename(
  filename: string,
): { kind: PreviewKind; mime: string } | null {
  const dot = filename.lastIndexOf('.')
  if (dot <= 0 || dot === filename.length - 1) {
    return null
  }
  return EXTENSION_TYPE[filename.slice(dot + 1).toLowerCase()] ?? null
}

/**
 * Whether an attachment can be previewed on demand, how, and as what type.
 *
 * A declared `info.mimetype` is the authority — including a declaration we
 * refuse, which refuses the whole preview rather than falling through to the
 * weaker signals (otherwise an `m.video` labelled `text/html` would still open
 * a player over those bytes). A *generic* declaration carries no information
 * and does fall through: to the msgtype, then to the filename extension.
 *
 * Media with no `mxc://` url — a local echo mid-upload, or a malformed event —
 * is never previewable, matching `MediaAttachment`'s `downloadable` guard. An
 * absent `size` is treated as under the ceiling: refusing on a missing field
 * would drop previews for every sender that omits `info.size`, and a failed
 * fetch already renders as an error.
 */
export function previewPlan(media: ParsedMedia): PreviewPlan | null {
  if (media.url === null || !media.url.startsWith('mxc://')) {
    return null
  }
  if (media.kind === 'image' || media.kind === 'sticker') {
    // Never reached — `EventBody` routes those to MediaImage.
    return null
  }

  const declared =
    media.mimetype === undefined || media.mimetype.trim() === ''
      ? null
      : normalizeMime(media.mimetype)

  let kind: PreviewKind | null = null
  let contentType: string | null = null

  if (declared !== null && !isUnhelpfulMimetype(declared)) {
    kind = kindFromMimetype(declared)
    if (kind === null) {
      return null
    }
    // Audio and video reach the browser correctly typed already; the proxy
    // downgrades pdf and text, so those are restored from the declaration.
    contentType =
      kind === 'pdf' ? 'application/pdf' : kind === 'text' ? 'text/plain' : null
  } else {
    // Nothing usable was declared. The extension is the only signal that also
    // yields a MIME, so prefer it, and let the msgtype cover an extensionless
    // filename.
    const guessed = fromFilename(media.filename)
    if (guessed !== null) {
      kind = guessed.kind
      contentType = guessed.mime
    } else if (media.kind === 'audio' || media.kind === 'video') {
      // A player needs *some* type. Without an extension the best available is
      // the container these msgtypes almost always carry.
      kind = media.kind
      contentType = media.kind === 'audio' ? 'audio/mpeg' : 'video/mp4'
    }
  }

  if (kind === null) {
    return null
  }
  if (media.size !== undefined && media.size > SIZE_CEILING[kind]) {
    return null
  }
  return { kind, contentType }
}

/** The byte ceiling for a kind — exported for the text reader's own cap. */
export function previewSizeCeiling(kind: PreviewKind): number {
  return SIZE_CEILING[kind]
}
