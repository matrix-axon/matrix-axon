/**
 * Naming an image format from its leading bytes.
 *
 * A direct port of the TUI's `sniff_format` (`clients/tui/src/app/media.rs`),
 * which has seen production media. Keep the two in step, for the same reason
 * `parse-media.ts` says so: cross-client divergence in media handling is a
 * recurring bug source.
 *
 * The point is to replace a *guess* with a *fact*. ADR 0101 had to decide,
 * after an `<img>` failed, whether the bytes were an undecodable format or the
 * proxy's ciphertext-fallback 200, and it could only weigh the sender's
 * declared `info.mimetype` against the filename extension — both
 * sender-controlled, neither describing the bytes that actually arrived. That
 * reasoning was ruled out of scope there because "the caller holds an object
 * URL, not the buffer", which is true of `MediaImage` but not of
 * `media-service`, where the `Blob` itself is in hand.
 */

/**
 * Bytes needed by the longest test below — the ISO-BMFF brand at offset 8..12.
 * Sniffing slices only this much off the front, never the whole object.
 */
export const SNIFF_HEAD_BYTES = 16

function starts(head: Uint8Array, magic: readonly number[]): boolean {
  if (head.length < magic.length) {
    return false
  }
  return magic.every((byte, i) => head[i] === byte)
}

function ascii(text: string): readonly number[] {
  return Array.from(text, (character) => character.charCodeAt(0))
}

function at(head: Uint8Array, start: number, text: string): boolean {
  const magic = ascii(text)
  if (head.length < start + magic.length) {
    return false
  }
  return magic.every((byte, i) => head[start + i] === byte)
}

/**
 * The format these bytes actually are, or `null` when they match no image
 * container we know.
 *
 * `null` means only that: unidentified. Ciphertext takes this shape, since
 * AES-CTR "succeeds" on the wrong key and yields uniform noise — but so does a
 * JSON error body served with a 200, and so does any format this table does
 * not carry. So `null` is never reported to the reader as a decryption
 * failure; callers fall back to the sender's declared type
 * (`imageDecodeFailureMessage`) and withhold Download
 * (`isSaveableAfterDecodeFailure`), which protects them without asserting
 * anything the client cannot know.
 */
export function sniffImageFormat(head: Uint8Array): string | null {
  if (starts(head, [0xff, 0xd8, 0xff])) {
    return 'JPEG'
  }
  if (starts(head, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])) {
    return 'PNG'
  }
  if (at(head, 0, 'GIF87a') || at(head, 0, 'GIF89a')) {
    return 'GIF'
  }
  if (at(head, 0, 'RIFF') && at(head, 8, 'WEBP')) {
    return 'WebP'
  }
  if (at(head, 0, 'BM')) {
    return 'BMP'
  }
  if (
    starts(head, [0x49, 0x49, 0x2a, 0x00]) ||
    starts(head, [0x4d, 0x4d, 0x00, 0x2a])
  ) {
    return 'TIFF'
  }
  // ISO Base Media File Format container: AVIF, HEIC, HEIF, MP4, …
  if (at(head, 4, 'ftyp')) {
    if (at(head, 8, 'avif') || at(head, 8, 'avis')) {
      return 'AVIF'
    }
    if (
      at(head, 8, 'heic') ||
      at(head, 8, 'heis') ||
      at(head, 8, 'heim') ||
      at(head, 8, 'heix')
    ) {
      return 'HEIC'
    }
    if (at(head, 8, 'mif1') || at(head, 8, 'msf1')) {
      return 'HEIF'
    }
    return 'ISO BMFF'
  }
  if (at(head, 0, '<svg') || at(head, 0, '<?xml') || at(head, 0, '<SVG')) {
    return 'SVG'
  }
  // JPEG 2000, in both shapes: the JP2 container box and a bare codestream.
  // Carried because `MIME_FORMATS` names this format from metadata, and a
  // sniffer that did not know it would answer `null` for bytes the declared
  // type identifies perfectly well — losing both the name and Download.
  if (starts(head, [0x00, 0x00, 0x00, 0x0c, 0x6a, 0x50, 0x20, 0x20])) {
    return 'JPEG 2000'
  }
  if (starts(head, [0xff, 0x4f, 0xff, 0x51])) {
    return 'JPEG 2000'
  }
  if (starts(head, [0x00, 0x00, 0x01, 0x00])) {
    return 'ICO'
  }
  return null
}

/**
 * Formats worth *naming* to the reader after a decode failure, keyed by what
 * [`sniffImageFormat`] returns.
 *
 * The rest — PNG, JPEG, GIF, WebP, BMP, ICO — are formats every target browser
 * decodes, so failing on one says nothing about the format and everything
 * about the moment: a revoked object URL, memory pressure, a decoder that gave
 * up. Naming the format there would be a true statement that misleads, so
 * those fall through to the transient wording instead.
 *
 * AVIF sits with the named group deliberately: Safari only decodes it from 16,
 * and an older iPhone failing on one is a format problem, not a transient one.
 */
export const SNIFFED_UNRENDERABLE: ReadonlySet<string> = new Set([
  'HEIC',
  'HEIF',
  'TIFF',
  'AVIF',
  'SVG',
  'JPEG 2000',
  'ISO BMFF',
])
