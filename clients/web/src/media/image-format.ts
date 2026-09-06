import type { SniffedFormat } from './media-service'
import type { ParsedMedia } from './parse-media'
import { SNIFFED_UNRENDERABLE } from './sniff'

/**
 * Naming the reason an image would not decode (ADR 0101).
 *
 * ADR 0101 was written against the belief that the proxy returns raw ciphertext
 * with a 200 when it holds no decryption key, so a decode failure *could* mean
 * undecryptable media. Treating that as the *only* cause is what made every
 * HEIC photo from an iPhone report itself as a decryption failure — Chromium
 * and Gecko decode no HEIC at all, WebKit does, and the bytes were fine the
 * whole time.
 *
 * Tracing the proxy since (#359) found even the premise near-unreachable: it
 * answers 404 while an event is undecrypted, and once decrypted the AES key is
 * *inside* `content.file.key`, so the server cannot lack it — a failure there
 * is a 502, never bytes. Ciphertext with a 200 needs an event that points at an
 * encrypted object through a *plaintext* descriptor, which is malformed rather
 * than merely keyless.
 *
 * These helpers only ever run *after* a real decode failure, which is what
 * lets the wording be definite: if we are here, this browser did try and could
 * not. A format listed below is not universally unsupported (WebKit reads HEIC
 * and TIFF quite happily) — it is a format that, having just failed, is worth
 * naming rather than blaming on encryption.
 *
 * Issue #359 added the third cause ADR 0101 did not have: bytes that are a
 * perfectly good PNG the browser simply fumbled. Blaming encryption for those
 * is the same mistake in a new place, so the wording now follows the *bytes*
 * where they are known, and only calls something ciphertext when it matches no
 * image container at all.
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
 * `application/octet-stream` or nothing at all. Both are sender-controlled, so
 * both are guesses.
 *
 * The bytes are no longer out of reach — `media-service` sniffs every object's
 * head (`sniff.ts`, the port of the TUI's `sniff_format`) — so callers that
 * have a verdict should pass it and let it win. ADR 0101 ruled sniffing out
 * here because "the caller holds an object URL, not the buffer", which was
 * true of this module's callers and never of the service that fetched the
 * bytes in the first place. This tier remains for the case where no sniff
 * could run.
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
 * Prefers what the *bytes* say over what the sender declared, then falls back
 * to the sender when the bytes say nothing usable:
 *
 * - a sniffed format the browser is entitled to refuse → name it;
 * - a sniffed format every target browser decodes → the format is not the
 *   problem and neither is encryption. Something went wrong *this time*: a
 *   revoked object URL, memory pressure, a decoder that gave up. Say so, and
 *   let the caller offer Retry;
 * - no usable verdict — bytes matching nothing `sniff.ts` knows, or no sniff at
 *   all — → ADR 0101's declared-mimetype-then-extension tiers, then a generic
 *   admission of ignorance.
 *
 * **Nothing here claims the server failed to decrypt.** From this side that is
 * indistinguishable from a format the sniffer does not carry and from a JSON
 * error body served with a 200 — so it was a guess, and stating it as fact sent
 * a real investigation at a server whose media pipeline was provably healthy
 * (issue #359). The server is the only party that can know, and when it does
 * know it answers 404 or 502 rather than handing over bytes.
 */
export function imageDecodeFailureMessage(
  media: ParsedMedia,
  sniffed?: SniffedFormat,
): string {
  if (sniffed !== undefined && sniffed !== null) {
    return SNIFFED_UNRENDERABLE.has(sniffed)
      ? `${sniffed} image — this browser can't display it`
      : "Image didn't load"
  }
  const format = unrenderableImageFormat(media)
  if (format !== null) {
    return `${format} image — this browser can't display it`
  }
  return 'Could not display this image'
}
