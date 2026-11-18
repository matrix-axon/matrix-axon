import { describe, expect, it, vi } from 'vitest'
import type { ApiClient } from './api/client'
import {
  createLinkedEventCache,
  linkedEventTargets,
  type LinkedEvent,
} from './linked-events'
import type { RoomDto } from './stores/room-list'

const event = {
  event_id: '$linked',
  room_id: '!room:hs',
  sender: '@bob:hs',
  origin_ts: 1,
  type: 'm.room.message',
  content: { msgtype: 'm.text', body: 'hello' },
  body: 'hello',
  relates_to: null,
  redacted: false,
  edited: false,
  sender_trust: null,
  source: 'store',
} as unknown as LinkedEvent

function fakeApi(answers: Array<() => Promise<unknown>>) {
  const GET = vi.fn(() => answers.shift()!())
  return { api: { GET } as unknown as ApiClient, GET }
}

const ok = () =>
  Promise.resolve({ data: { data: event }, response: { status: 200 } })
const status = (code: number) => () =>
  Promise.resolve({ data: undefined, response: { status: code } })

describe('the linked-event cache (ADR 0138 § 5)', () => {
  it('keeps an event for the session and shares concurrent reads', async () => {
    const { api, GET } = fakeApi([ok])
    const cache = createLinkedEventCache(api)
    const [a, b] = await Promise.all([
      cache.read('acct', '!room:hs', '$linked'),
      cache.read('acct', '!room:hs', '$linked'),
    ])
    expect(a).toEqual({ kind: 'event', event })
    expect(b).toBe(a)
    expect(await cache.read('acct', '!room:hs', '$linked')).toEqual(a)
    expect(GET).toHaveBeenCalledTimes(1)
    expect(GET).toHaveBeenCalledWith(
      '/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}',
      {
        params: {
          path: {
            account_id: 'acct',
            room_id: '!room:hs',
            event_id: '$linked',
          },
        },
      },
    )
  })

  it('keeps a 404 but reads a transient failure again', async () => {
    const { api, GET } = fakeApi([
      status(404),
      status(503),
      () => Promise.reject(new Error('offline')),
      ok,
    ])
    const cache = createLinkedEventCache(api)
    expect(await cache.read('acct', '!r', '$gone')).toEqual({ kind: 'gone' })
    expect(await cache.read('acct', '!r', '$gone')).toEqual({ kind: 'gone' })
    expect(await cache.read('acct', '!r', '$e')).toEqual({ kind: 'transient' })
    expect(await cache.read('acct', '!r', '$e')).toEqual({ kind: 'transient' })
    expect((await cache.read('acct', '!r', '$e')).kind).toBe('event')
    expect(GET).toHaveBeenCalledTimes(4)
  })

  it('reads past the cache when asked, and replaces what it kept', async () => {
    const { api, GET } = fakeApi([ok, status(404)])
    const cache = createLinkedEventCache(api)
    await cache.read('acct', '!room:hs', '$linked')
    const [fresh, again] = await Promise.all([
      cache.read('acct', '!room:hs', '$linked', { fresh: true }),
      cache.read('acct', '!room:hs', '$linked', { fresh: true }),
    ])
    expect(fresh).toEqual({ kind: 'gone' })
    expect(again).toBe(fresh)
    expect(await cache.read('acct', '!room:hs', '$linked')).toEqual({
      kind: 'gone',
    })
    expect(GET).toHaveBeenCalledTimes(2)
  })

  it('forgets everything on clear, including a read in flight', async () => {
    let release!: () => void
    const held = new Promise<void>((resolve) => (release = resolve))
    const { api, GET } = fakeApi([() => held.then(ok), ok])
    const cache = createLinkedEventCache(api)
    const before = cache.read('acct', '!room:hs', '$linked')
    cache.clear()
    release()
    await before
    await cache.read('acct', '!room:hs', '$linked')
    expect(GET).toHaveBeenCalledTimes(2)
  })
})

describe('linkedEventTargets (ADR 0138 § 2)', () => {
  const context = {
    accountId: 'acct',
    rooms: [
      { account_id: 'acct', room_id: '!a:hs', canonical_alias: '#a:hs' },
    ] as RoomDto[],
  }
  const link = (room: string, id: string) =>
    `https://matrix.to/#/${encodeURIComponent(room)}/${encodeURIComponent(id)}`

  it('takes event links in text order, distinct, at most three, never itself', () => {
    const body = [
      link('!a:hs', '$self'),
      link('!a:hs', '$one'),
      'https://matrix.to/#/@user:hs',
      'https://matrix.to/#/!a:hs',
      link('#a:hs', '$one'),
      link('!b:hs', '$two'),
      'https://example.org/',
      link('!c:hs', '$three'),
      link('!d:hs', '$four'),
    ].join(' ')
    expect(
      linkedEventTargets(
        { event_id: '$self', content: { msgtype: 'm.text', body }, body },
        context,
      ),
    ).toEqual([
      { roomId: '!a:hs', eventId: '$one', via: [] },
      { roomId: '!b:hs', eventId: '$two', via: [] },
      { roomId: '!c:hs', eventId: '$three', via: [] },
    ])
  })

  it('reads the formatted body without its reply fallback', () => {
    const content = {
      msgtype: 'm.text',
      format: 'org.matrix.custom.html',
      formatted_body: `<mx-reply><a href="${link('!a:hs', '$quoted')}">quoted</a></mx-reply>See <a href="${link('!a:hs', '$shown')}">this</a>`,
    }
    expect(
      linkedEventTargets({ event_id: '$m', content, body: 'x' }, context),
    ).toEqual([{ roomId: '!a:hs', eventId: '$shown', via: [] }])
  })

  it('drops a plain reply fallback and a message with a spoiler', () => {
    const body = `> <@bob:hs> ${link('!a:hs', '$quoted')}\n\n${link('!a:hs', '$shown')}`
    expect(
      linkedEventTargets(
        {
          event_id: '$m',
          content: {
            msgtype: 'm.text',
            body,
            'm.relates_to': { 'm.in_reply_to': { event_id: '$q' } },
          },
          body,
        },
        context,
      ),
    ).toEqual([{ roomId: '!a:hs', eventId: '$shown', via: [] }])
    expect(
      linkedEventTargets(
        {
          event_id: '$m',
          content: {
            format: 'org.matrix.custom.html',
            formatted_body: `<span data-mx-spoiler>${link('!a:hs', '$hidden')}</span>`,
          },
          body: 'x',
        },
        context,
      ),
    ).toEqual([])
  })
})
