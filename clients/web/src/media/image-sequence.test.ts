import { describe, expect, it } from 'vitest'
import type { TimelineEvent } from '../stores/timeline'
import { imageSequence } from './image-sequence'

function event(
  eventId: string,
  content: Record<string, unknown> | null,
  overrides: Partial<TimelineEvent> = {},
): TimelineEvent {
  return {
    account_id: '11111111-1111-4111-8111-111111111111',
    event_id: eventId,
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

function image(eventId: string, ts: number, url = `mxc://hs/${eventId}`) {
  return event(
    eventId,
    { msgtype: 'm.image', body: 'p.png', url },
    { origin_ts: ts },
  )
}

function text(eventId: string, ts: number) {
  return event(eventId, { msgtype: 'm.text', body: 'hello' }, { origin_ts: ts })
}

describe('imageSequence', () => {
  it('is empty for a timeline with no media', () => {
    expect(imageSequence([text('$a', 1), text('$b', 2)])).toEqual([])
  })

  it('keeps images in the order given, skipping non-media', () => {
    const seq = imageSequence([
      image('$1', 10),
      text('$t', 20),
      image('$2', 30),
      image('$3', 40),
    ])
    expect(seq.map((i) => i.eventId)).toEqual(['$1', '$2', '$3'])
    expect(seq.map((i) => i.ts)).toEqual([10, 30, 40])
  })

  it('includes stickers', () => {
    const seq = imageSequence([
      event('$s', { body: 'wave', url: 'mxc://hs/s1' }, { type: 'm.sticker' }),
    ])
    expect(seq.map((i) => i.eventId)).toEqual(['$s'])
    expect(seq[0].media.kind).toBe('sticker')
  })

  it('excludes still-uploading echoes', () => {
    // Their bytes are an object url the composer revokes on reconcile, so a
    // viewer holding one could have it pulled out from under it.
    const pending = event('$echo', null, {
      origin_ts: 50,
      arrival_order: 50,
      localEcho: {
        status: 'pending',
        body: 'cat.png',
        options: {},
        media: {
          file: new File(['x'], 'cat.png', { type: 'image/png' }),
          kind: 'image',
          previewUrl: 'blob:preview',
        },
      },
    } as Partial<TimelineEvent>)
    expect(
      imageSequence([image('$1', 10), pending]).map((i) => i.eventId),
    ).toEqual(['$1'])
  })

  it('excludes non-image media', () => {
    const file = event('$f', {
      msgtype: 'm.file',
      body: 'notes.pdf',
      url: 'mxc://hs/f1',
    })
    expect(
      imageSequence([file, image('$1', 10)]).map((i) => i.eventId),
    ).toEqual(['$1'])
  })

  it('excludes an image whose url is not mxc', () => {
    expect(imageSequence([image('$1', 10, 'https://hs/a.png')])).toEqual([])
  })

  it('carries accountId and media through for the viewer', () => {
    const [item] = imageSequence([image('$1', 10)])
    expect(item.accountId).toBe('11111111-1111-4111-8111-111111111111')
    expect(item.media.url).toBe('mxc://hs/$1')
  })
})
