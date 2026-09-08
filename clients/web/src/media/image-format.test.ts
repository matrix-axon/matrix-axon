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

  it('never blames decryption, even for unidentifiable encrypted bytes', () => {
    // The client cannot tell ciphertext from a format it does not carry, so it
    // does not guess (#359). Withholding Download is the protection; accusing
    // the server was never evidence-backed.
    expect(imageDecodeFailureMessage(media({ encrypted: true }))).toBe(
      'Could not display this image',
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

describe('imageDecodeFailureMessage, with sniffed bytes', () => {
  it('blames the moment when the bytes are a format browsers render', () => {
    // The regression this was filed for (#359): a valid PNG that WebKit
    // fumbled reported itself as a server-side decryption failure, and a
    // healthy server got investigated for it.
    expect(imageDecodeFailureMessage(media({ encrypted: true }), 'PNG')).toBe(
      "Image didn't load",
    )
    expect(imageDecodeFailureMessage(media(), 'JPEG')).toBe("Image didn't load")
  })

  it('names a format the browser is entitled to refuse', () => {
    expect(imageDecodeFailureMessage(media({ encrypted: true }), 'HEIC')).toBe(
      "HEIC image — this browser can't display it",
    )
  })

  it('trusts the bytes over a sender who declared the wrong type', () => {
    // `info.mimetype` and the filename are both sender-controlled; the bytes
    // are not. A HEIC announced as a PNG is still a HEIC.
    expect(
      imageDecodeFailureMessage(
        media({ mimetype: 'image/png', filename: 'holiday.png' }),
        'HEIC',
      ),
    ).toBe("HEIC image — this browser can't display it")
    // And the converse: a real PNG announced as a HEIC is not a format problem.
    expect(
      imageDecodeFailureMessage(media({ mimetype: 'image/heic' }), 'PNG'),
    ).toBe("Image didn't load")
  })

  it('falls back to the declared type when the bytes match nothing known', () => {
    // The regression this catches: `sniff.ts` carries no DNG, so a valid
    // encrypted DNG sniffed as `null`. Reading that as "ciphertext" both
    // accused the server and withheld the Download that used to be offered.
    const dng = media({ mimetype: 'image/x-adobe-dng', encrypted: true })
    expect(imageDecodeFailureMessage(dng, null)).toBe(
      "DNG image — this browser can't display it",
    )
  })

  it('stays generic for bytes nothing at all identifies', () => {
    expect(imageDecodeFailureMessage(media({ encrypted: true }), null)).toBe(
      'Could not display this image',
    )
    expect(imageDecodeFailureMessage(media(), null)).toBe(
      'Could not display this image',
    )
  })

  it('falls back to the declared type when no sniff ran', () => {
    // `undefined` is the absence of evidence, not evidence of absence — a Blob
    // that could not produce its head. ADR 0101's tiers still apply.
    expect(
      imageDecodeFailureMessage(
        media({ mimetype: 'image/heic', encrypted: true }),
        undefined,
      ),
    ).toBe("HEIC image — this browser can't display it")
  })
})
