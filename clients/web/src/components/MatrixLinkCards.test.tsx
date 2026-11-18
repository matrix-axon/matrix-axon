import { render, screen, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import type { TimelineEvent } from '../stores/timeline'
import { TEST_BASE_URL, testServices } from '../test/services'
import { EventBody } from './EventBody'
import { TRUST_NOTICES } from './MatrixLinkCards'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'
const LINKED = `${TEST_BASE_URL}/v1/accounts/:account/rooms/:room/events/:event`
const link = 'https://matrix.to/#/!other%3Ahs/%24linked?via=hs'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function message(body = `Look at ${link}`): TimelineEvent {
  return {
    account_id: ACCOUNT,
    room_id: '!room:hs',
    event_id: '$message',
    sender: '@adam:hs',
    origin_ts: 1,
    arrival_order: 1,
    type: 'm.room.message',
    state_key: null,
    body,
    redacted: false,
    content: { msgtype: 'm.text', body },
  } as unknown as TimelineEvent
}

function linked(overrides: Record<string, unknown> = {}) {
  return {
    event_id: '$linked',
    room_id: '!other:hs',
    sender: '@bob:hs',
    origin_ts: 1_700_000_000_000,
    type: 'm.room.message',
    content: { msgtype: 'm.text', body: 'The linked message' },
    body: 'The linked message',
    relates_to: null,
    redacted: false,
    edited: false,
    sender_trust: null,
    source: 'homeserver',
    ...overrides,
  }
}

function serve(answer: () => Response, seen: string[] = []) {
  server.use(
    http.get(LINKED, ({ params }) => {
      seen.push(`${String(params.room)} ${String(params.event)}`)
      return answer()
    }),
  )
  return seen
}

function view(event = message(), services = testServices({ token: 't' })) {
  render(
    <ServicesContext.Provider value={services}>
      <EventBody event={event} />
    </ServicesContext.Provider>,
  )
  return services
}

describe('Matrix-link cards (ADR 0138)', () => {
  it('shows the linked message, read with the recipient account', async () => {
    const seen = serve(() => HttpResponse.json({ data: linked() }))
    view()
    expect(await screen.findByText('The linked message')).toBeTruthy()
    expect(screen.getByText('@bob:hs')).toBeTruthy()
    expect(seen).toEqual(['!other:hs $linked'])
    const card = document.querySelector<HTMLAnchorElement>('.matrix-link-card')!
    // Not a room this account has joined: the link leads to the join flow.
    expect(card.href).toContain('https://matrix.to/#/')
  })

  it.each(Object.entries(TRUST_NOTICES))(
    'says plainly when the sender trust is %s',
    async (verdict, notice) => {
      serve(() =>
        HttpResponse.json({ data: linked({ sender_trust: verdict }) }),
      )
      view()
      expect(await screen.findByText(notice)).toBeTruthy()
      expect(screen.getByText('The linked message')).toBeTruthy()
    },
  )

  it('adds nothing for a verified or unverified sender', async () => {
    serve(() =>
      HttpResponse.json({ data: linked({ sender_trust: 'unverified' }) }),
    )
    view()
    await screen.findByText('The linked message')
    expect(document.querySelector('.matrix-link-card-trust')).toBeNull()
  })

  it('reads a deleted and an undecryptable message as such', async () => {
    serve(() =>
      HttpResponse.json({
        data: linked({ redacted: true, content: null, body: null }),
      }),
    )
    view()
    expect(await screen.findByText('Message deleted')).toBeTruthy()
  })

  it('leaves no card for a 404 and keeps it for the session', async () => {
    const seen = serve(() =>
      HttpResponse.json({ error: { code: 'not_found' } }, { status: 404 }),
    )
    const services = view()
    await waitFor(() => expect(seen).toHaveLength(1))
    expect(document.querySelector('.matrix-link-card')).toBeNull()
    view(message(), services)
    await new Promise((resolve) => setTimeout(resolve, 20))
    expect(seen).toHaveLength(1)
  })

  it('reads nothing when the setting is off', async () => {
    const seen = serve(() => HttpResponse.json({ data: linked() }))
    const services = testServices({ token: 't' })
    services.settings.matrixLinkCards.value = false
    view(message(), services)
    await new Promise((resolve) => setTimeout(resolve, 20))
    expect(seen).toEqual([])
    expect(document.querySelector('.room-pill')).not.toBeNull()
  })

  it('makes no card for links to users or rooms', async () => {
    const seen = serve(() => HttpResponse.json({ data: linked() }))
    view(
      message(
        'https://matrix.to/#/@bob:hs and https://matrix.to/#/!other%3Ahs?via=hs',
      ),
    )
    await new Promise((resolve) => setTimeout(resolve, 20))
    expect(seen).toEqual([])
  })

  it('reads the image through the event that names it', async () => {
    serve(() =>
      HttpResponse.json({
        data: linked({
          content: {
            msgtype: 'm.image',
            body: 'cat.png',
            url: 'mxc://hs/cat',
            info: { mimetype: 'image/png' },
          },
          body: 'cat.png',
        }),
      }),
    )
    const media: string[] = []
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
        ({ request }) => {
          media.push(new URL(request.url).search)
          return new HttpResponse(new Uint8Array([0x89, 0x50, 0x4e, 0x47]), {
            headers: { 'content-type': 'image/png' },
          })
        },
      ),
    )
    view()
    await waitFor(() => expect(media).toHaveLength(1))
    const query = new URLSearchParams(media[0])
    expect(query.get('room_id')).toBe('!other:hs')
    expect(query.get('event_id')).toBe('$linked')
  })

  it('re-reads the event once when its image is a 404, then asks again', async () => {
    const image = linked({
      content: {
        msgtype: 'm.image',
        body: 'cat.png',
        url: 'mxc://hs/cat',
        info: { mimetype: 'image/png' },
      },
      body: 'cat.png',
    })
    const reads = serve(() => HttpResponse.json({ data: image }))
    let media = 0
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media/thumbnail`,
        () => {
          media += 1
          return HttpResponse.json(
            { error: { code: 'not_found' } },
            { status: 404 },
          )
        },
      ),
    )
    view()
    await waitFor(() => expect(media).toBe(2))
    expect(reads).toHaveLength(2)
    // No further reads or image requests: the card stays without its image.
    await new Promise((resolve) => setTimeout(resolve, 30))
    expect(media).toBe(2)
    expect(reads).toHaveLength(2)
    expect(screen.getByText('@bob:hs')).toBeTruthy()
    expect(document.querySelector('.matrix-link-card-image')).toBeNull()
  })
})
