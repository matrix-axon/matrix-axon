import type { ParsedMedia } from './parse-media'

/**
 * Naming the reason an image would not decode (ADR 0101).
 *
 * The proxy returns raw ciphertext with a 200 when it holds no decryption key,
 * so a decode failure *can* mean undecryptable media — but it is not the only
 * cause, and treating it as the only cause is what made every HEIC photo from
 * an iPhone report itself as a decryption failure. Chromium and Gecko decode
 * no HEIC at all; WebKit does. The bytes were fine the whole time.
 *
 * These helpers only ever run *after* a real decode failure, which is what
 * lets the wording be definite: if we are here, this browser did try and could
 * not. A format listed below is not universally unsupported (WebKit reads HEIC
 * and TIFF quite happily) — it is a format that, having just failed, is worth
 * naming rather than blaming on encryption.
 */

/**
 * Formats a browser may refuse, mapped to the name to show. Camera output,
 * mostly: HEIC is the iPhone default, and the raw formats arrive from people
 * sending originals off a real camera.
 *
 * Deliberately a closed table rather than a guess. Anything not listed falls
 * through to the generic message — a wrong name is worse than no name, since
 * naming a format is the part the reader will act on.
 */
const MIME_FORMATS: Record<string, string> = {
  'image/heic': 'HEIC',
  'image/heic-sequence': 'HEIC',
  'image/heif': 'HEIF',
  'image/heif-sequence': 'HEIF',
  'image/tiff': 'TIFF',
  'image/jp2': 'JPEG 2000',
  'image/jpx': 'JPEG 2000',
  'image/jpm': 'JPEG 2000',
  'image/x-adobe-dng': 'DNG',
  'image/x-canon-cr2': 'camera raw',
  'image/x-canon-cr3': 'camera raw',
  'image/x-nikon-nef': 'camera raw',
  'image/x-sony-arw': 'camera raw',
}

/** The same table by filename extension, for senders that declare no type. */
const EXTENSION_FORMATS: Record<string, string> = {
  heic: 'HEIC',
  heics: 'HEIC',
  heif: 'HEIF',
  heifs: 'HEIF',
  hif: 'HEIF',
  tif: 'TIFF',
  tiff: 'TIFF',
  jp2: 'JPEG 2000',
  j2k: 'JPEG 2000',
  jpf: 'JPEG 2000',
  jpx: 'JPEG 2000',
  dng: 'camera raw',
  cr2: 'camera raw',
  cr3: 'camera raw',
  nef: 'camera raw',
  arw: 'camera raw',
  orf: 'camera raw',
  raf: 'camera raw',
  rw2: 'camera raw',
}

/**
 * The display name of a format this browser has just failed to decode, or
 * `null` when nothing identifies it.
 *
 * Two tiers, following ADR 0072's `previewPlan()`: a declared `info.mimetype`
 * first, then the filename extension for the many senders that declare
 * `application/octet-stream` or nothing at all. Unlike `previewPlan()` this
 * cannot be given the bytes — the caller holds an object URL, not the buffer —
 * so magic-byte sniffing (which the TUI does in `sniff_format`,
 * `clients/tui/src/app/media.rs`) is deliberately out of scope here and noted
 * in ADR 0101 as belonging with the decoding work, which needs the bytes
 * anyway.
 */
export function unrenderableImageFormat(media: ParsedMedia): string | null {
  const mimetype = media.mimetype?.trim().toLowerCase()
  if (mimetype !== undefined && mimetype !== '') {
    const byMime = MIME_FORMATS[mimetype]
    if (byMime !== undefined) {
      return byMime
    }
    // A specific image type we do not list simply failed for some other
    // reason. Say nothing rather than guess from the extension, which for
    // `photo.heic.jpg` would name the wrong format. A generic declaration
    // (`application/octet-stream`, the shape ADR 0072 found on real events)
    // is not specific, so it falls through to the extension below.
    if (mimetype.startsWith('image/')) {
      return null
    }
  }
  const dot = media.filename.lastIndexOf('.')
  if (dot < 0) {
    return null
  }
  const extension = media.filename.slice(dot + 1).toLowerCase()
  return EXTENSION_FORMATS[extension] ?? null
}

/**
 * What to tell the reader when an image's bytes arrived but would not decode.
 *
 * Three outcomes, in the order they are worth knowing, mirroring the condition
 * the TUI has always used (`decode_image`, `clients/tui/src/app/media.rs`):
 * name the format when we can, fall back to encryption only for media that is
 * actually encrypted, and otherwise admit to not knowing.
 */
export function imageDecodeFailureMessage(media: ParsedMedia): string {
  const format = unrenderableImageFormat(media)
  if (format !== null) {
    return `${format} image — this browser can't display it`
  }
  if (media.encrypted) {
    return 'Encrypted media — server could not decrypt'
  }
  return 'Could not display this image'
}
