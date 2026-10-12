import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { LocationProvider } from 'preact-iso'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import { TEST_BASE_URL, testServices } from '../test/services'
import { FormattedBody } from './FormattedBody'
import type { RoomDto } from '../stores/room-list'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'
const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47])

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  window.history.replaceState(null, '', '/')
})
afterAll(() => server.close())

function room(overrides: Partial<RoomDto>): RoomDto {
  return {
    account_id: ACCOUNT,
    account_user_id: '@me:hs',
    room_id: '!room:hs',
    name: null,
    canonical_alias: null,
    topic: null,
    last_activity_ts: 0,
    last_event_id: null,
    membership: 'join',
    ...overrides,
  } as RoomDto
}

async function servicesWithRooms(rooms: RoomDto[]) {
  const services = testServices()
  server.use(
    http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
      HttpResponse.json({ data: rooms }),
    ),
  )
  await services.rooms.refresh()
  return services
}

function renderBody(
  props: { body?: string | null; content: unknown },
  services = testServices(),
) {
  return render(
    <ServicesContext.Provider value={services}>
      <FormattedBody
        accountId={ACCOUNT}
        body={props.body}
        content={props.content}
      />
    </ServicesContext.Provider>,
  )
}

describe('FormattedBody plain-text linkification', () => {
  it('turns bare URLs into safe anchors', () => {
    const { container } = renderBody({
      body: 'see https://example.org/x?q=1 for details',
      content: null,
    })
    const anchor = container.querySelector('a')!
    expect(anchor.getAttribute('href')).toBe('https://example.org/x?q=1')
    expect(anchor.getAttribute('target')).toBe('_blank')
    expect(anchor.getAttribute('rel')).toBe('noopener noreferrer')
    expect(container.textContent).toBe(
      'see https://example.org/x?q=1 for details',
    )
  })

  it('leaves plain URL anchors available for the modern message-link style', () => {
    const { container } = renderBody({
      body: 'see https://example.org/x?q=1',
      content: null,
    })
    const anchor = container.querySelector('a')!
    expect(anchor.className).toBe('')
    expect(anchor.closest('.event-body')).toBeNull()
    expect(anchor.closest('.body-text')).toBeTruthy()
  })

  it('trims trailing sentence punctuation from the link', () => {
    const { container } = renderBody({
      body: 'read this: https://example.org/page.',
      content: null,
    })
    expect(container.querySelector('a')!.getAttribute('href')).toBe(
      'https://example.org/page',
    )
    expect(container.textContent).toBe('read this: https://example.org/page.')
  })

  it('linkifies multiple URLs and leaves other text alone', () => {
    const { container } = renderBody({
      body: 'a https://one.example and http://two.example b',
      content: null,
    })
    const hrefs = [...container.querySelectorAll('a')].map((a) =>
      a.getAttribute('href'),
    )
    expect(hrefs).toEqual(['https://one.example', 'http://two.example'])
  })

  it.each([
    ['matrix:r/ops%3Aexample.org?via=example.org', 'Join #ops:example.org'],
    ['matrix:r/a:bostoncoop.net', 'Join #a:bostoncoop.net'],
    ['matrix:room/a:bostoncoop.net', 'Join #a:bostoncoop.net'],
  ])(
    'renders unknown matrix: room URI %s as a join pill in plain text',
    (href, label) => {
      const { container } = renderBody({
        body: `join ${href}`,
        content: null,
      })
      const anchor = container.querySelector('a')!
      expect(anchor.className).toBe('room-pill join-pill')
      expect(anchor.getAttribute('href')).toBe(href)
      expect(anchor.getAttribute('target')).toBeNull()
      expect(anchor.getAttribute('rel')).toBeNull()
      expect(anchor.getAttribute('title')).toBe('Join room')
      expect(anchor.textContent).toBe(label)
    },
  )

  it('does not linkify unsupported schemes or plain text', () => {
    const { container } = renderBody({
      body: 'javascript:alert(1) and ftp://x and words',
      content: null,
    })
    expect(container.querySelector('a')).toBeNull()
  })

  it('adds emoji-name tooltips to emoji in plain message text', () => {
    const { container } = renderBody({
      body: 'ship it 👍',
      content: null,
    })

    const tooltip = container.querySelector('.emoji-tooltip')!
    expect(tooltip.textContent).toBe('👍')
    expect(tooltip.getAttribute('title')).toBe('thumbs up')
    expect(container.textContent).toBe('ship it 👍')
  })

  it('keeps plain-text links intact while adding emoji tooltips around them', () => {
    const { container } = renderBody({
      body: '👍 https://example.org 😄',
      content: null,
    })

    expect(container.querySelector('a')!.textContent).toBe(
      'https://example.org',
    )
    expect(
      [...container.querySelectorAll('.emoji-tooltip')].map((el) =>
        el.getAttribute('title'),
      ),
    ).toEqual(['thumbs up', 'grinning face with smiling eyes'])
  })

  it('linkifies bare URLs inside formatted bodies too (Element parity)', () => {
    const { container } = renderBody({
      body: 'fallback',
      content: {
        format: 'org.matrix.custom.html',
        formatted_body: 'try this: https://example.org<br>second line',
      },
    })
    expect(container.querySelector('a')!.getAttribute('href')).toBe(
      'https://example.org',
    )
  })

  it('adds emoji-name tooltips to emoji in formatted message text', () => {
    const { container } = renderBody({
      body: 'ship it 👍',
      content: {
        format: 'org.matrix.custom.html',
        formatted_body: '<strong>ship it 👍</strong>',
      },
    })

    const tooltip = container.querySelector('.emoji-tooltip')!
    expect(tooltip.textContent).toBe('👍')
    expect(tooltip.getAttribute('title')).toBe('thumbs up')
    expect(container.querySelector('strong')?.textContent).toBe('ship it 👍')
  })

  it('does not add emoji wrappers inside formatted links or code', () => {
    const { container } = renderBody({
      body: '👍',
      content: {
        format: 'org.matrix.custom.html',
        formatted_body:
          '<a href="https://example.org">👍</a> <code>😄</code> 👍',
      },
    })

    expect(container.querySelector('a .emoji-tooltip')).toBeNull()
    expect(container.querySelector('code .emoji-tooltip')).toBeNull()
    expect(container.querySelectorAll('.emoji-tooltip')).toHaveLength(1)
  })

  it('flags formatted links whose visible URL points somewhere else', () => {
    const { container } = renderBody({
      body: 'fallback',
      content: {
        format: 'org.matrix.custom.html',
        formatted_body: '<a href="https://example.com">https://google.com</a>',
      },
    })
    const anchor = container.querySelector('a')!

    expect(anchor.className).toBe('dangerous-link')
    expect(anchor.getAttribute('title')).toContain('opens https://example.com')
    expect(anchor.getAttribute('aria-label')).toContain(
      'warning: opens https://example.com',
    )
  })

  it('turns known Matrix.to room URLs in plain text into local room pills', async () => {
    const services = await servicesWithRooms([
      room({
        room_id: '!ops:hs',
        name: 'Ops',
        canonical_alias: '#ops:hs',
      }),
    ])
    const { container } = renderBody(
      {
        body: 'visit https://matrix.to/#/%23ops%3Ahs',
        content: null,
      },
      services,
    )

    const anchor = container.querySelector('a')!
    expect(anchor.className).toBe('room-pill')
    expect(anchor.getAttribute('href')).toBe(`/${ACCOUNT}/rooms/!ops%3Ahs`)
    expect(anchor.getAttribute('target')).toBeNull()
    expect(anchor.getAttribute('title')).toBe('Open existing room')
    expect(anchor.textContent).toBe('Ops')
  })

  it('turns known matrix:room URIs in plain text into local room pills', async () => {
    const services = await servicesWithRooms([
      room({
        room_id: '!ops:bostoncoop.net',
        name: 'Ops',
        canonical_alias: '#a:bostoncoop.net',
      }),
    ])
    const { container } = renderBody(
      {
        body: 'visit matrix:room/a:bostoncoop.net',
        content: null,
      },
      services,
    )

    const anchor = container.querySelector('a')!
    expect(anchor.className).toBe('room-pill')
    expect(anchor.getAttribute('href')).toBe(
      `/${ACCOUNT}/rooms/!ops%3Abostoncoop.net`,
    )
    expect(anchor.getAttribute('target')).toBeNull()
    expect(anchor.getAttribute('title')).toBe('Open existing room')
    expect(anchor.textContent).toBe('Ops')
  })

  it('turns Matrix.to user URLs in plain text into mention pills', () => {
    const { container } = renderBody({
      body: 'ping https://matrix.to/#/%40alice%3Ahs',
      content: null,
    })

    const anchor = container.querySelector('a')!
    expect(anchor.className).toBe('mention-pill')
    expect(anchor.getAttribute('href')).toBe('/rooms/dm?user=%40alice%3Ahs')
    expect(anchor.getAttribute('target')).toBeNull()
    expect(anchor.getAttribute('title')).toBe('Open DM with @alice:hs')
    expect(anchor.textContent).toBe('@alice')
  })

  it('opens Matrix.to user pills as Axon DMs', async () => {
    const createdRoom = '!dm:hs'
    let dmBody: unknown = null
    const services = await servicesWithRooms([
      room({ room_id: '!ops:hs', name: 'Ops', canonical_alias: '#ops:hs' }),
    ])
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`,
        async ({ request }) => {
          dmBody = await request.json()
          return HttpResponse.json({ data: { room_id: createdRoom } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            room({
              room_id: createdRoom,
              name: null,
              canonical_alias: null,
            }),
          ],
        }),
      ),
    )
    const { container } = renderBody(
      {
        body: 'ping https://matrix.to/#/%40alice%3Ahs',
        content: null,
      },
      services,
    )

    fireEvent.click(container.querySelector('a')!)

    await waitFor(() => expect(dmBody).toEqual({ user_id: '@alice:hs' }))
    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(createdRoom)}`,
      ),
    )
  })

  it('opens an existing DM from a user pill without visiting the start-DM form', async () => {
    const dmRoom = '!existing-dm:hs'
    let dmCalls = 0
    const services = await servicesWithRooms([
      room({
        room_id: dmRoom,
        name: 'Alice',
        canonical_alias: null,
        is_direct: true,
      }),
    ])
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(dmRoom)}/members`,
        () =>
          HttpResponse.json({
            data: [
              {
                user_id: '@me:hs',
                membership: 'join',
                display_name: 'Me',
              },
              {
                user_id: '@alice:hs',
                membership: 'join',
                display_name: 'Alice',
              },
            ],
          }),
      ),
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`, () => {
        dmCalls += 1
        return HttpResponse.json({ data: { room_id: '!new-dm:hs' } })
      }),
    )
    const { container } = render(
      <LocationProvider>
        <ServicesContext.Provider value={services}>
          <FormattedBody
            accountId={ACCOUNT}
            body="ping https://matrix.to/#/%40alice%3Ahs"
            content={null}
          />
        </ServicesContext.Provider>
      </LocationProvider>,
    )

    fireEvent.click(container.querySelector('a')!)

    expect(window.location.pathname).toBe('/')
    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(dmRoom)}`,
      ),
    )
    expect(dmCalls).toBe(0)
  })

  it('opens the start-DM form when a user pill cannot open or create a room', async () => {
    const services = await servicesWithRooms([
      room({ room_id: '!ops:hs', name: 'Ops', canonical_alias: '#ops:hs' }),
    ])
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`, () =>
        HttpResponse.json(
          { error: { code: 'forbidden', message: 'blocked' } },
          { status: 403 },
        ),
      ),
    )
    const { container } = render(
      <LocationProvider>
        <ServicesContext.Provider value={services}>
          <FormattedBody
            accountId={ACCOUNT}
            body="ping https://matrix.to/#/%40alice%3Ahs"
            content={null}
          />
        </ServicesContext.Provider>
      </LocationProvider>,
    )

    fireEvent.click(container.querySelector('a')!)

    expect(window.location.pathname).toBe('/')
    await waitFor(() => {
      expect(window.location.pathname).toBe('/rooms/dm')
      expect(window.location.search).toBe('?user=%40alice%3Ahs')
    })
  })

  it('turns known Matrix.to room anchors in formatted bodies into local room pills', async () => {
    const services = await servicesWithRooms([
      room({
        room_id: '!ops:hs',
        name: 'Ops',
        canonical_alias: '#ops:hs',
      }),
    ])
    const { container } = renderBody(
      {
        body: 'fallback',
        content: {
          format: 'org.matrix.custom.html',
          formatted_body:
            '<a href="https://matrix.to/#/%23ops%3Ahs">#ops:hs</a>',
        },
      },
      services,
    )

    const anchor = container.querySelector('a')!
    expect(anchor.className).toBe('room-pill')
    expect(anchor.getAttribute('href')).toBe(`/${ACCOUNT}/rooms/!ops%3Ahs`)
    expect(anchor.getAttribute('target')).toBeNull()
    expect(anchor.getAttribute('title')).toBe('Open existing room')
    expect(anchor.textContent).toBe('#ops:hs')
  })

  it('turns unknown matrix:room anchors in formatted bodies into join pills', () => {
    const { container } = renderBody({
      body: 'fallback',
      content: {
        format: 'org.matrix.custom.html',
        formatted_body:
          '<a href="matrix:room/a:bostoncoop.net">matrix:room/a:bostoncoop.net</a>',
      },
    })

    const anchor = container.querySelector('a')!
    expect(anchor.className).toBe('room-pill join-pill')
    expect(anchor.getAttribute('href')).toBe('matrix:room/a:bostoncoop.net')
    expect(anchor.getAttribute('target')).toBeNull()
    expect(anchor.getAttribute('title')).toBe('Join room')
    expect(anchor.textContent).toBe('Join #a:bostoncoop.net')
  })

  it('turns Matrix.to room-event anchors into local event deep links', async () => {
    const services = await servicesWithRooms([
      room({
        room_id: '!ops:hs',
        name: 'Ops',
        canonical_alias: '#ops:hs',
      }),
    ])
    const { container } = renderBody(
      {
        body: 'fallback',
        content: {
          format: 'org.matrix.custom.html',
          formatted_body:
            '<a href="https://matrix.to/#/%23ops%3Ahs/%24event?via=hs">#ops:hs</a>',
        },
      },
      services,
    )

    const anchor = container.querySelector('a')!
    expect(anchor.className).toBe('room-pill event-pill')
    expect(anchor.getAttribute('href')).toBe(
      `/${ACCOUNT}/rooms/!ops%3Ahs?event=%24event`,
    )
    expect(anchor.getAttribute('target')).toBeNull()
    expect(anchor.getAttribute('title')).toBe(
      'Open existing room at this message',
    )
  })

  it('turns Matrix.to user anchors in formatted bodies into mention pills', () => {
    const { container } = renderBody({
      body: 'fallback',
      content: {
        format: 'org.matrix.custom.html',
        formatted_body: '<a href="https://matrix.to/#/@alice:hs">@alice:hs</a>',
      },
    })

    const anchor = container.querySelector('a')!
    expect(anchor.className).toBe('mention-pill')
    expect(anchor.getAttribute('href')).toBe('/rooms/dm?user=%40alice%3Ahs')
    expect(anchor.getAttribute('target')).toBeNull()
    expect(anchor.getAttribute('title')).toBe('Open DM with @alice:hs')
    expect(anchor.textContent).toBe('@alice:hs')
  })

  it('turns unknown Matrix.to room URLs into join pills', () => {
    const { container } = renderBody({
      body: 'visit https://matrix.to/#/%23unknown%3Ahs',
      content: null,
    })

    const anchor = container.querySelector('a')!
    expect(anchor.className).toBe('room-pill join-pill')
    expect(anchor.getAttribute('href')).toBe(
      'https://matrix.to/#/%23unknown%3Ahs',
    )
    expect(anchor.getAttribute('target')).toBeNull()
    expect(anchor.getAttribute('title')).toBe('Join room')
    expect(anchor.textContent).toBe('Join #unknown:hs')
  })

  it('joins unknown Matrix.to room URLs from plain text inside Axon', async () => {
    const joinedRoom = '!joined:hs'
    let joinBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: joinedRoom } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [room({ room_id: joinedRoom })] }),
      ),
    )
    const { container } = renderBody({
      body: 'visit https://matrix.to/#/%23unknown%3Ahs?via=hs',
      content: null,
    })

    fireEvent.click(container.querySelector('a')!)

    await waitFor(() =>
      expect(joinBody).toEqual({
        room_id_or_alias: '#unknown:hs',
        server_names: ['hs'],
      }),
    )
    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(joinedRoom)}`,
      ),
    )
  })

  it('joins Matrix.to room ID URLs that rely on via hints inside Axon', async () => {
    const roomId = '!Oha01wS8odafKACi1LVh_QLxBRJVaMYRCEbISYUJYKk'
    const joinedRoom = '!joined:bostoncoop.net'
    let joinBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: joinedRoom } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [room({ room_id: joinedRoom })] }),
      ),
    )
    const { container } = renderBody({
      body: `visit https://matrix.to/#/${roomId}?via=bostoncoop.net`,
      content: null,
    })

    const anchor = container.querySelector('a')!
    expect(anchor.className).toBe('room-pill join-pill')
    expect(anchor.getAttribute('target')).toBeNull()
    expect(anchor.textContent).toBe(`Join ${roomId}`)

    fireEvent.click(anchor)

    await waitFor(() =>
      expect(joinBody).toEqual({
        room_id_or_alias: roomId,
        server_names: ['bostoncoop.net'],
      }),
    )
    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(joinedRoom)}`,
      ),
    )
  })

  it('joins unknown Matrix.to room anchors from formatted bodies inside Axon', async () => {
    const joinedRoom = '!joined:hs'
    let joinBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: joinedRoom } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [room({ room_id: joinedRoom })] }),
      ),
    )
    const { container } = renderBody({
      body: 'fallback',
      content: {
        format: 'org.matrix.custom.html',
        formatted_body:
          '<a href="https://matrix.to/#/%23unknown%3Ahs?via=hs">#unknown:hs</a>',
      },
    })

    fireEvent.click(container.querySelector('a')!)

    await waitFor(() =>
      expect(joinBody).toEqual({
        room_id_or_alias: '#unknown:hs',
        server_names: ['hs'],
      }),
    )
    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(joinedRoom)}`,
      ),
    )
  })
})

describe('FormattedBody inline media (ADR 0064)', () => {
  it('resolves an inline data-mxc image to a blob src after mount', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/:account/:server/:media`,
        () =>
          new HttpResponse(PNG, { headers: { 'content-type': 'image/png' } }),
      ),
    )
    const { container } = renderBody({
      body: 'a picture',
      content: {
        format: 'org.matrix.custom.html',
        formatted_body: '<img src="mxc://hs/inline" alt="inline">',
      },
    })
    const img = container.querySelector('img')!
    // The sanitizer stashed the mxc in data-mxc and dropped src.
    expect(img.getAttribute('data-mxc')).toBe('mxc://hs/inline')
    await waitFor(() => expect(img.getAttribute('src')).toMatch(/^blob:/))
  })
})
