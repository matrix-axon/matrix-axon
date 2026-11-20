import { describe, expect, it } from 'vitest'
import type { ParsedMedia } from './parse-media'
import { previewPlan } from './preview-kind'

function media(overrides: Partial<ParsedMedia> = {}): ParsedMedia {
  return {
    kind: 'file',
    url: 'mxc://hs/doc',
    thumbnailUrl: null,
    filename: 'doc.pdf',
    caption: null,
    encrypted: false,
    mimetype: 'application/pdf',
    size: 1536,
    ...overrides,
  }
}

/** The resolved kind alone — most cases do not care about the re-typed MIME. */
function kindOf(parsed: ParsedMedia) {
  return previewPlan(parsed)?.kind ?? null
}

describe('previewPlan', () => {
  it('classifies a pdf attachment', () => {
    expect(kindOf(media())).toBe('pdf')
  })

  it('classifies text and text-ish application types', () => {
    expect(kindOf(media({ mimetype: 'text/plain' }))).toBe('text')
    expect(kindOf(media({ mimetype: 'text/markdown' }))).toBe('text')
    expect(kindOf(media({ mimetype: 'application/json' }))).toBe('text')
  })

  it('ignores MIME parameters and case', () => {
    expect(kindOf(media({ mimetype: 'TEXT/Plain; charset=utf-8' }))).toBe(
      'text',
    )
  })

  it('falls back to the msgtype when the filename has no extension', () => {
    expect(
      kindOf(media({ kind: 'audio', filename: 'memo', mimetype: undefined })),
    ).toBe('audio')
    expect(
      kindOf(media({ kind: 'video', filename: 'clip', mimetype: undefined })),
    ).toBe('video')
  })

  it('prefers the extension over the msgtype when they disagree', () => {
    // Neither signal is authoritative, but only the extension also yields a
    // MIME the player can use, and a name is the more specific claim.
    expect(
      kindOf(
        media({ kind: 'audio', filename: 'notes.txt', mimetype: undefined }),
      ),
    ).toBe('text')
  })

  it('refuses active types even when the sender declares them', () => {
    expect(kindOf(media({ mimetype: 'text/html' }))).toBeNull()
    expect(kindOf(media({ mimetype: 'image/svg+xml' }))).toBeNull()
    // …including on a msgtype that would otherwise be trusted.
    expect(kindOf(media({ kind: 'video', mimetype: 'text/html' }))).toBe(null)
  })

  it('refuses types with no inline renderer', () => {
    expect(kindOf(media({ mimetype: 'application/zip' }))).toBeNull()
    expect(
      kindOf(media({ filename: 'archive.zip', mimetype: undefined })),
    ).toBeNull()
  })

  it('falls back to the filename extension when no mimetype is declared', () => {
    expect(kindOf(media({ filename: 'clip.MOV', mimetype: undefined }))).toBe(
      'video',
    )
    expect(kindOf(media({ filename: 'memo.m4a', mimetype: undefined }))).toBe(
      'audio',
    )
    expect(kindOf(media({ filename: 'notes.md', mimetype: undefined }))).toBe(
      'text',
    )
  })

  it('treats a generic binary mimetype as no mimetype at all', () => {
    // Senders that could not determine a type say octet-stream; the msgtype
    // and the extension are then the only signals left.
    expect(
      kindOf(
        media({
          kind: 'video',
          filename: 'clip.mov',
          mimetype: 'application/octet-stream',
        }),
      ),
    ).toBe('video')
    expect(
      kindOf(
        media({ filename: 'memo.m4a', mimetype: 'application/octet-stream' }),
      ),
    ).toBe('audio')
  })

  it('lets a declared type refuse a preview the extension would allow', () => {
    // A positive statement about the bytes wins over the filename.
    expect(
      kindOf(media({ filename: 'invoice.pdf', mimetype: 'text/html' })),
    ).toBeNull()
    expect(
      kindOf(media({ filename: 'clip.mov', mimetype: 'application/zip' })),
    ).toBeNull()
  })

  it('refuses media with no mxc url', () => {
    expect(kindOf(media({ url: null }))).toBeNull()
    expect(kindOf(media({ url: 'https://hs/doc.pdf' }))).toBeNull()
  })

  it('refuses images and stickers — those route to MediaImage', () => {
    expect(kindOf(media({ kind: 'image', mimetype: 'image/png' }))).toBe(null)
    expect(kindOf(media({ kind: 'sticker', mimetype: 'image/png' }))).toBe(null)
  })

  it('refuses an object over its size ceiling', () => {
    expect(kindOf(media({ size: 200 * 1024 * 1024 }))).toBeNull()
    expect(
      kindOf(media({ mimetype: 'text/plain', size: 600 * 1024 })),
    ).toBeNull()
  })

  it('allows a phone-sized video — the ceiling is the upload cap', () => {
    expect(
      kindOf(
        media({
          kind: 'video',
          filename: 'clip.mov',
          mimetype: 'video/quicktime',
          size: 80 * 1024 * 1024,
        }),
      ),
    ).toBe('video')
  })

  it('allows media whose size the sender omitted', () => {
    expect(kindOf(media({ size: undefined }))).toBe('pdf')
  })
})

describe('previewPlan re-typing', () => {
  it('restores the type the proxy downgrades', () => {
    // `is_inline_safe` serves pdf and text as application/octet-stream.
    expect(previewPlan(media())?.contentType).toBe('application/pdf')
    expect(previewPlan(media({ mimetype: 'text/plain' }))?.contentType).toBe(
      'text/plain',
    )
  })

  it('keeps the served type for correctly declared audio and video', () => {
    // The proxy already sends these through with their real Content-Type.
    expect(
      previewPlan(media({ kind: 'audio', mimetype: 'audio/mpeg' }))
        ?.contentType,
    ).toBeNull()
    expect(
      previewPlan(media({ kind: 'video', mimetype: 'video/mp4' }))?.contentType,
    ).toBeNull()
  })

  it('types a generic-mimetype video from its extension', () => {
    // The real event from a phone video sent through the web client: an
    // m.file with an octet-stream mimetype. A <video> will not play a blob
    // typed application/octet-stream, so the extension supplies the type.
    const plan = previewPlan(
      media({
        kind: 'file',
        filename: 'IMG_9306.mov',
        mimetype: 'application/octet-stream',
        size: 2_360_866,
      }),
    )
    expect(plan).toEqual({
      kind: 'video',
      contentType: 'video/quicktime',
      stream: false,
    })
  })

  it('types a generic-mimetype voice memo from its extension', () => {
    // Same event shape, and note the filename comes from `body` here — the
    // sender set no explicit `filename` (parseMedia falls back to it).
    const plan = previewPlan(
      media({
        kind: 'file',
        filename: 'Tafthill Terr 5.m4a',
        mimetype: 'application/octet-stream',
        size: 19_191,
      }),
    )
    expect(plan).toEqual({
      kind: 'audio',
      contentType: 'audio/mp4',
      stream: false,
    })
  })

  it('never re-types to something that could carry active content', () => {
    // Every reachable contentType must be inert: a `.html`/`.svg` attachment
    // has no entry, so it has no preview and no re-type.
    for (const filename of ['x.html', 'x.htm', 'x.svg', 'x.xhtml']) {
      expect(
        previewPlan(media({ filename, mimetype: 'application/octet-stream' })),
      ).toBeNull()
    }
  })

  describe('streaming (ADR 0139 § 6)', () => {
    const video = (overrides: Partial<ParsedMedia> = {}) =>
      media({
        kind: 'video',
        filename: 'clip.mp4',
        mimetype: 'video/mp4',
        ...overrides,
      })
    const streamed = (parsed: ParsedMedia) =>
      previewPlan(parsed, { streaming: true })

    it('streams video and audio the proxy serves as their own type', () => {
      expect(streamed(video())).toEqual({
        kind: 'video',
        contentType: null,
        stream: true,
      })
      expect(
        streamed(
          media({ kind: 'audio', filename: 'a.ogg', mimetype: 'audio/ogg' }),
        )?.stream,
      ).toBe(true)
    })

    it('never streams where the platform cannot', () => {
      expect(previewPlan(video())?.stream).toBe(false)
      expect(previewPlan(video(), { streaming: false })?.stream).toBe(false)
    })

    it('lifts the size ceiling only for a plan that streams', () => {
      const huge = video({ size: 1024 * 1024 * 1024 })
      expect(streamed(huge)).toEqual({
        kind: 'video',
        contentType: null,
        stream: true,
      })
      // The blob path still buffers the whole object, so it keeps its cap.
      expect(previewPlan(huge)).toBeNull()
    })

    it('keeps the blob path for a type the proxy would not serve as media', () => {
      // Each of these is served as application/octet-stream
      // (`sanitize_mime`/`is_inline_safe`), which the ticket route refuses —
      // so the blob path re-types it, and its ceiling stays.
      for (const mimetype of [
        'application/octet-stream',
        'video/mp4; codecs="avc1"',
        'Video/mp4',
      ]) {
        const plan = streamed(video({ mimetype, size: 200 * 1024 * 1024 }))
        expect(plan).toBeNull()
        expect(streamed(video({ mimetype }))?.stream).toBe(false)
      }
    })

    it('streams a mixed-case subtype, which the proxy serves as itself', () => {
      // `sanitize_mime` keeps the subtype's case and `is_inline_safe` checks
      // only the lowercase major type, so `video/MP4` is served as-is.
      expect(streamed(video({ mimetype: 'video/MP4' }))?.stream).toBe(true)
    })

    it('never streams a pdf or text file', () => {
      expect(streamed(media())?.stream).toBe(false)
      expect(streamed(media({ mimetype: 'text/plain' }))?.stream).toBe(false)
    })
  })
})
