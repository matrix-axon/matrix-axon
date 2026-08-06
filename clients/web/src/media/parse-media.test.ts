import { describe, expect, it } from 'vitest'
import type { EventDto } from '../stores/timeline'
import { mxcToPath, parseMedia } from './parse-media'

function event(
  content: Record<string, unknown> | null,
  type = 'm.room.message',
): EventDto {
  return {
    account_id: '11111111-1111-4111-8111-111111111111',
    event_id: '$e',
    room_id: '!r:hs',
    sender: '@alice:hs',
    origin_ts: 0,
    arrival_order: 0,
    type,
    body: null,
    content,
    redacted: false,
    edited: false,
    edit_count: 0,
    state_key: null,
    reactions: null,
    relates_to: null,
  } as EventDto
}

describe('parseMedia', () => {
  it('returns null for a plain text message', () => {
    expect(parseMedia(event({ msgtype: 'm.text', body: 'hi' }))).toBeNull()
  })

  it('parses a plain m.image with dimensions and mimetype', () => {
    const media = parseMedia(
      event({
        msgtype: 'm.image',
        body: 'cat.png',
        url: 'mxc://hs/abc',
        info: { mimetype: 'image/png', size: 1234, w: 800, h: 600 },
      }),
    )
    expect(media).toMatchObject({
      kind: 'image',
      url: 'mxc://hs/abc',
      filename: 'cat.png',
      caption: null,
      encrypted: false,
      mimetype: 'image/png',
      size: 1234,
      w: 800,
      h: 600,
    })
  })

  it('reads the sticker kind from the event type, not the msgtype', () => {
    const media = parseMedia(
      event({ body: 'wave', url: 'mxc://hs/stk' }, 'm.sticker'),
    )
    expect(media?.kind).toBe('sticker')
  })

  it('drops an image whose url is not an mxc URI', () => {
    expect(
      parseMedia(event({ msgtype: 'm.image', url: 'https://evil/x.png' })),
    ).toBeNull()
    expect(parseMedia(event({ msgtype: 'm.image' }))).toBeNull()
  })

  it('marks an encrypted image and resolves file.url', () => {
    const media = parseMedia(
      event({
        msgtype: 'm.image',
        body: 'secret.png',
        file: { url: 'mxc://hs/enc' },
        info: { mimetype: 'image/png' },
      }),
    )
    expect(media).toMatchObject({ url: 'mxc://hs/enc', encrypted: true })
  })

  it('keeps body as a caption only when a distinct filename is present', () => {
    const withCaption = parseMedia(
      event({
        msgtype: 'm.image',
        filename: 'IMG_0001.jpg',
        body: 'sunset over the bay',
        url: 'mxc://hs/img',
      }),
    )
    expect(withCaption).toMatchObject({
      filename: 'IMG_0001.jpg',
      caption: 'sunset over the bay',
    })

    const noCaption = parseMedia(
      event({
        msgtype: 'm.image',
        filename: 'IMG_0001.jpg',
        body: 'IMG_0001.jpg',
        url: 'mxc://hs/img',
      }),
    )
    expect(noCaption?.caption).toBeNull()
  })

  it('parses file/audio/video without requiring an mxc url', () => {
    const file = parseMedia(
      event({
        msgtype: 'm.file',
        body: 'report.pdf',
        url: 'mxc://hs/doc',
        info: { mimetype: 'application/pdf', size: 4096 },
      }),
    )
    expect(file).toMatchObject({ kind: 'file', filename: 'report.pdf' })
  })

  it('uses an Element-X PDF body as the caption when filename is distinct', () => {
    const media = parseMedia(
      event({
        body: 'Seems bad',
        file: { url: 'mxc://bostoncoop.net/pByvspuicZsxIJPdDOlMzsyt' },
        filename: 'Firefox.pdf',
        info: { mimetype: 'application/pdf', size: 2077976 },
        msgtype: 'm.file',
      }),
    )

    expect(media).toMatchObject({
      kind: 'file',
      url: 'mxc://bostoncoop.net/pByvspuicZsxIJPdDOlMzsyt',
      filename: 'Firefox.pdf',
      caption: 'Seems bad',
      encrypted: true,
      mimetype: 'application/pdf',
      size: 2077976,
    })
  })

  it('reads a sender-embedded thumbnail url', () => {
    const media = parseMedia(
      event({
        msgtype: 'm.image',
        url: 'mxc://hs/full',
        info: { thumbnail_url: 'mxc://hs/thumb' },
      }),
    )
    expect(media?.thumbnailUrl).toBe('mxc://hs/thumb')
  })

  it('reads an encrypted thumbnail from thumbnail_file.url', () => {
    const media = parseMedia(
      event({
        msgtype: 'm.image',
        file: { url: 'mxc://hs/full' },
        info: { thumbnail_file: { url: 'mxc://hs/thumb' } },
      }),
    )
    expect(media?.thumbnailUrl).toBe('mxc://hs/thumb')
  })

  it('falls back to the "media" filename when neither field is set', () => {
    expect(parseMedia(event({ msgtype: 'm.file' }))?.filename).toBe('media')
  })

  it('returns null for a UTD (null content)', () => {
    expect(parseMedia(event(null))).toBeNull()
  })
})

describe('mxcToPath', () => {
  it('splits server and media id', () => {
    expect(mxcToPath('mxc://matrix.org/AbC123')).toEqual({
      serverName: 'matrix.org',
      mediaId: 'AbC123',
    })
  })

  it('rejects a non-mxc or malformed uri', () => {
    expect(mxcToPath('https://x/y')).toBeNull()
    expect(mxcToPath('mxc://only-server')).toBeNull()
    expect(mxcToPath('mxc:///no-server')).toBeNull()
    expect(mxcToPath('mxc://server/')).toBeNull()
  })
})
