import { describe, expect, it } from 'vitest'
import {
  imageDecodeFailureMessage,
  unrenderableImageFormat,
} from './image-format'
import type { ParsedMedia } from './parse-media'

function media(overrides: Partial<ParsedMedia> = {}): ParsedMedia {
  return {
    kind: 'image',
    url: 'mxc://hs/full',
    thumbnailUrl: null,
    filename: 'cat.png',
    caption: null,
    encrypted: false,
    mimetype: 'image/png',
    ...overrides,
  }
}

describe('unrenderableImageFormat', () => {
  it('names a declared HEIC', () => {
    expect(
      unrenderableImageFormat(
        media({ mimetype: 'image/heic', filename: 'IMG_1.HEIC' }),
      ),
    ).toBe('HEIC')
  })

  it('is case- and whitespace-insensitive about the declaration', () => {
    expect(unrenderableImageFormat(media({ mimetype: '  IMAGE/HEIF  ' }))).toBe(
      'HEIF',
    )
  })

  it('falls back to the extension for a generic declaration', () => {
    // The shape ADR 0072 found on real phone media: no usable media type, the
    // filename carrying the only signal.
    expect(
      unrenderableImageFormat(
        media({ mimetype: 'application/octet-stream', filename: 'IMG_2.heic' }),
      ),
    ).toBe('HEIC')
  })

  it('falls back to the extension when nothing was declared at all', () => {
    expect(
      unrenderableImageFormat(
        media({ mimetype: undefined, filename: 'scan.tiff' }),
      ),
    ).toBe('TIFF')
  })

  it('trusts a specific image declaration over a misleading extension', () => {
    // `photo.heic.jpg` is a transcoded file whose old extension survived in
    // the middle of the name. Naming HEIC there would be actively wrong.
    expect(
      unrenderableImageFormat(
        media({ mimetype: 'image/jpeg', filename: 'photo.heic.jpg' }),
      ),
    ).toBeNull()
  })

  it('names nothing for an ordinary image, whatever the failure was', () => {
    expect(unrenderableImageFormat(media())).toBeNull()
  })

  it('names nothing for an extensionless filename', () => {
    expect(
      unrenderableImageFormat(
        media({ mimetype: undefined, filename: 'media' }),
      ),
    ).toBeNull()
  })
})

describe('imageDecodeFailureMessage', () => {
  it('names the format when it can, encrypted or not', () => {
    const expected = "HEIC image — this browser can't display it"
    expect(imageDecodeFailureMessage(media({ mimetype: 'image/heic' }))).toBe(
      expected,
    )
    // An encrypted HEIC decrypted just fine; the format is still the reason.
    expect(
      imageDecodeFailureMessage(
        media({ mimetype: 'image/heic', encrypted: true }),
      ),
    ).toBe(expected)
  })

  it('blames decryption only for encrypted media it cannot identify', () => {
    expect(imageDecodeFailureMessage(media({ encrypted: true }))).toBe(
      'Encrypted media — server could not decrypt',
    )
  })

  it('does not blame decryption for plaintext media', () => {
    // The regression ADR 0101 was filed for: this was the message every
    // undecodable image got, including unencrypted ones.
    expect(imageDecodeFailureMessage(media())).toBe(
      'Could not display this image',
    )
  })
})
