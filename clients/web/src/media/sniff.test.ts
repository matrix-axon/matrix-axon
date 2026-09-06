import { describe, expect, it } from 'vitest'
import { SNIFFED_UNRENDERABLE, sniffImageFormat } from './sniff'

/** The leading bytes of each container, as they arrive off the wire. */
function head(...bytes: number[]): Uint8Array {
  return new Uint8Array(bytes)
}

function ascii(text: string, ...trailing: number[]): Uint8Array {
  return new Uint8Array([
    ...Array.from(text, (character) => character.charCodeAt(0)),
    ...trailing,
  ])
}

describe('sniffImageFormat', () => {
  it('names the formats a browser renders', () => {
    expect(sniffImageFormat(head(0xff, 0xd8, 0xff, 0xe0))).toBe('JPEG')
    expect(
      sniffImageFormat(head(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)),
    ).toBe('PNG')
    expect(sniffImageFormat(ascii('GIF89a'))).toBe('GIF')
    expect(sniffImageFormat(ascii('RIFF????WEBP'))).toBe('WebP')
    expect(sniffImageFormat(ascii('BM'))).toBe('BMP')
    expect(sniffImageFormat(head(0x00, 0x00, 0x01, 0x00))).toBe('ICO')
  })

  it('names the camera formats that actually reach a timeline', () => {
    // An iPhone photo sent as an original — the case ADR 0101 was written for.
    expect(sniffImageFormat(ascii('????ftypheic'))).toBe('HEIC')
    expect(sniffImageFormat(ascii('????ftypmif1'))).toBe('HEIF')
    expect(sniffImageFormat(ascii('????ftypavif'))).toBe('AVIF')
    expect(sniffImageFormat(head(0x49, 0x49, 0x2a, 0x00))).toBe('TIFF')
    expect(sniffImageFormat(head(0x4d, 0x4d, 0x00, 0x2a))).toBe('TIFF')
  })

  it('falls back to the container for an unfamiliar ISO-BMFF brand', () => {
    // A video, or a brand postdating this table. Naming the container beats
    // claiming the bytes are unrecognizable, which would imply ciphertext.
    expect(sniffImageFormat(ascii('????ftypmp42'))).toBe('ISO BMFF')
  })

  it('returns null for bytes matching no image container', () => {
    // AES-CTR against the wrong key "succeeds" and yields uniform noise. This
    // null is the only honest evidence that the proxy served ciphertext.
    expect(
      sniffImageFormat(head(0x3f, 0x91, 0xd2, 0x0a, 0x7c, 0x44)),
    ).toBeNull()
    // A JSON error body served with a 200 looks the same from here, which is
    // why the caller still qualifies the claim with `media.encrypted`.
    expect(sniffImageFormat(ascii('{"error":'))).toBeNull()
  })

  it('returns null rather than guessing from a truncated signature', () => {
    // Four bytes of a PNG signature is not a PNG. Deciding otherwise would
    // make a short read look like a successful identification.
    expect(sniffImageFormat(head(0x89, 0x50, 0x4e, 0x47))).toBeNull()
    expect(sniffImageFormat(new Uint8Array())).toBeNull()
  })

  it('lists only formats a browser is entitled to refuse', () => {
    // The point of the split: PNG and JPEG failing says nothing about the
    // format, so they must not be namable as "this browser can't display it".
    for (const renderable of ['PNG', 'JPEG', 'GIF', 'WebP', 'BMP', 'ICO']) {
      expect(SNIFFED_UNRENDERABLE.has(renderable)).toBe(false)
    }
    for (const refusable of ['HEIC', 'HEIF', 'TIFF', 'SVG']) {
      expect(SNIFFED_UNRENDERABLE.has(refusable)).toBe(true)
    }
  })
})
