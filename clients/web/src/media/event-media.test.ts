import { describe, expect, it } from 'vitest'
import type { TimelineEvent } from '../stores/timeline'
import { eventMedia, isImageMedia } from './event-media'

function event(
  content: Record<string, unknown> | null,
  overrides: Partial<TimelineEvent> = {},
): TimelineEvent {
  return {
    account_id: '11111111-1111-4111-8111-111111111111',
    event_id: '$e',
    room_id: '!r:hs',
    sender: '@alice:hs',
    origin_ts: 0,
    arrival_order: 0,
    type: 'm.room.message',
    body: null,
    content,
    redacted: false,
    edited: false,
    edit_count: 0,
    state_key: null,
    reactions: null,
    relates_to: null,
    ...overrides,
  } as TimelineEvent
}

function echo(file: File, body: string, previewUrl: string | null = null) {
  return event(null, {
    localEcho: {
      status: 'pending',
      body,
      options: {},
      media: { file, kind: 'image', previewUrl },
    },
  } as Partial<TimelineEvent>)
}

const png = () => new File(['x'], 'cat.png', { type: 'image/png' })

describe('eventMedia', () => {
  it('returns null for a plain text message', () => {
    expect(eventMedia(event({ msgtype: 'm.text', body: 'hi' }))).toBeNull()
  })

  it('resolves a confirmed image as non-pending with no preview url', () => {
    const resolved = eventMedia(
      event({
        msgtype: 'm.image',
        body: 'cat.png',
        url: 'mxc://hs/abc',
        info: { mimetype: 'image/png', size: 1234, w: 800, h: 600 },
      }),
    )
    expect(resolved).not.toBeNull()
    expect(resolved!.pending).toBe(false)
    expect(resolved!.previewUrl).toBeNull()
    expect(resolved!.media).toMatchObject({
      kind: 'image',
      url: 'mxc://hs/abc',
      filename: 'cat.png',
    })
  })

  it('resolves a sticker from the event type', () => {
    const resolved = eventMedia(
      event({ body: 'wave', url: 'mxc://hs/s1' }, { type: 'm.sticker' }),
    )
    expect(resolved!.media.kind).toBe('sticker')
    expect(isImageMedia(resolved!.media)).toBe(true)
  })

  it('builds media for a still-uploading echo, which has no mxc url', () => {
    const resolved = eventMedia(echo(png(), 'cat.png', 'blob:preview'))
    expect(resolved).not.toBeNull()
    expect(resolved!.pending).toBe(true)
    expect(resolved!.previewUrl).toBe('blob:preview')
    expect(resolved!.media).toMatchObject({
      kind: 'image',
      url: null,
      filename: 'cat.png',
      encrypted: false,
      mimetype: 'image/png',
    })
  })

  it('treats an echo body equal to the filename as no caption', () => {
    expect(eventMedia(echo(png(), 'cat.png'))!.media.caption).toBeNull()
  })

  it('keeps an echo body that differs from the filename as the caption', () => {
    expect(eventMedia(echo(png(), 'look at this'))!.media.caption).toBe(
      'look at this',
    )
  })

  it('leaves an echo mimetype undefined when the File carries none', () => {
    const file = new File(['x'], 'blob.bin', { type: '' })
    expect(eventMedia(echo(file, 'blob.bin'))!.media.mimetype).toBeUndefined()
  })

  it('prefers the echo over event content when both are present', () => {
    const withContent = event(
      { msgtype: 'm.image', body: 'server.png', url: 'mxc://hs/abc' },
      {
        localEcho: {
          status: 'pending',
          body: 'cat.png',
          options: {},
          media: { file: png(), kind: 'image', previewUrl: null },
        },
      } as Partial<TimelineEvent>,
    )
    const resolved = eventMedia(withContent)
    expect(resolved!.pending).toBe(true)
    expect(resolved!.media.filename).toBe('cat.png')
  })

  it('resolves media on a redacted event — the caller owns that placeholder', () => {
    // `eventMedia` answers "what media is here", not "should it be drawn";
    // `EventBody` keeps the redaction check ahead of the confirmed-media
    // branch, so this returning non-null does not change what renders.
    const resolved = eventMedia(
      event(
        { msgtype: 'm.image', body: 'cat.png', url: 'mxc://hs/abc' },
        { redacted: true },
      ),
    )
    expect(resolved).not.toBeNull()
  })

  it('rejects an image whose url is not mxc', () => {
    expect(
      eventMedia(
        event({
          msgtype: 'm.image',
          body: 'cat.png',
          url: 'https://hs/abc.png',
        }),
      ),
    ).toBeNull()
  })
})

describe('isImageMedia', () => {
  it.each([
    ['image', true],
    ['sticker', true],
    ['file', false],
    ['audio', false],
    ['video', false],
  ] as const)('%s -> %s', (kind, expected) => {
    expect(isImageMedia({ kind } as never)).toBe(expected)
  })
})
