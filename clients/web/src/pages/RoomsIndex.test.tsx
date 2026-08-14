import {
  cleanup,
  fireEvent,
  render,
  waitFor,
  within,
} from '@testing-library/preact'
import { LocationProvider } from 'preact-iso'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import { TEST_BASE_URL, testServices } from '../test/services'
import { memoryStorage } from '../test/memory-storage'
import { RoomsIndex } from './RoomsIndex'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ACCOUNT_DTO = {
  account_id: ACCOUNT,
  user_id: '@alice:example.org',
  homeserver_url: 'https://matrix.example.org',
  state: 'active',
  verified: true,
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
}
const OTHER_ACCOUNT = '6b53f7f0-0000-4000-8000-000000000002'
const OTHER_ACCOUNT_DTO = {
  ...ACCOUNT_DTO,
  account_id: OTHER_ACCOUNT,
  user_id: '@bob:example.net',
  homeserver_url: 'https://matrix.example.net',
}

const server = setupServer(
  http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
    HttpResponse.json({ data: [ACCOUNT_DTO] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/rooms`, () => HttpResponse.json({ data: [] })),
  http.get(`${TEST_BASE_URL}/v1/invites`, () =>
    HttpResponse.json({ data: [] }),
  ),
)
let localStorageMock: Storage
let originalLocalStorage: PropertyDescriptor | undefined

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' })
  originalLocalStorage =
    Object.getOwnPropertyDescriptor(window, 'localStorage') ??
    Object.getOwnPropertyDescriptor(Window.prototype, 'localStorage')
})
afterEach(() => {
  cleanup()
  server.resetHandlers()
  localStorageMock?.clear()
  history.replaceState(null, '', '/')
})
afterAll(() => {
  server.close()
  if (originalLocalStorage !== undefined) {
    Object.defineProperty(window, 'localStorage', originalLocalStorage)
  }
})

describe('RoomsIndex discovery', () => {
  it('focuses direct join first and tabs to directory search on the discovery route', async () => {
    history.replaceState(null, '', '/rooms/discover')
    const { findByLabelText } = renderRoomsIndex()

    const roomInput = (await findByLabelText('Room')) as HTMLInputElement
    const searchInput = (await findByLabelText('Search')) as HTMLInputElement

    await waitFor(() => expect(document.activeElement).toBe(roomInput))

    fireEvent.keyDown(roomInput, { key: 'Tab' })

    expect(document.activeElement).toBe(searchInput)
  })

  it('focuses create room on the create route', async () => {
    history.replaceState(null, '', '/rooms/create')
    const { findByLabelText } = renderRoomsIndex()

    const nameInput = (await findByLabelText('Name')) as HTMLInputElement

    await waitFor(() => expect(document.activeElement).toBe(nameInput))
  })

  it('focuses direct message on the DM route', async () => {
    history.replaceState(null, '', '/rooms/dm')
    const { findByLabelText } = renderRoomsIndex()

    const userInput = (await findByLabelText('User')) as HTMLInputElement

    await waitFor(() => expect(document.activeElement).toBe(userInput))
  })

  it('focuses directory search from the Find room action target', async () => {
    history.replaceState(null, '', '/rooms/discover#find')
    const { findByLabelText } = renderRoomsIndex()

    const searchInput = (await findByLabelText('Search')) as HTMLInputElement

    await waitFor(() => expect(document.activeElement).toBe(searchInput))
  })

  it('honors room action targets over the route default', async () => {
    history.replaceState(null, '', '/rooms/discover#dm')
    const { findByLabelText } = renderRoomsIndex()

    const userInput = (await findByLabelText('User')) as HTMLInputElement

    await waitFor(() => expect(document.activeElement).toBe(userInput))
  })

  it('prefills direct message user from the DM route query', async () => {
    history.replaceState(null, '', '/rooms/dm?user=%40carol%3Aexample.org')
    const { findByLabelText } = renderRoomsIndex()

    const userInput = (await findByLabelText('User')) as HTMLInputElement

    await waitFor(() => expect(userInput.value).toBe('@carol:example.org'))
  })

  it('keeps normal tab order to Join when the direct room field has a target', async () => {
    history.replaceState(null, '', '/rooms/discover')
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    const roomInput = (await findByLabelText('Room')) as HTMLInputElement
    const joinButton = await findByRole('button', { name: 'Join' })
    roomInput.value = '#ops:example.org'

    fireEvent.keyDown(roomInput, { key: 'Tab' })

    expect(document.activeElement).toBe(joinButton)
  })

  it('shift-tabs from directory search back to the empty direct room field', async () => {
    history.replaceState(null, '', '/rooms/discover')
    const { findByLabelText, findByText } = renderRoomsIndex()

    await findByText('example.org')
    const roomInput = (await findByLabelText('Room')) as HTMLInputElement
    const searchInput = (await findByLabelText('Search')) as HTMLInputElement
    searchInput.focus()

    fireEvent.keyDown(searchInput, { key: 'Tab', shiftKey: true })

    expect(document.activeElement).toBe(roomInput)
  })

  it('shift-tabs from directory search back to Join when direct room has a target', async () => {
    history.replaceState(null, '', '/rooms/discover')
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    const roomInput = (await findByLabelText('Room')) as HTMLInputElement
    const searchInput = (await findByLabelText('Search')) as HTMLInputElement
    const joinButton = await findByRole('button', { name: 'Join' })
    roomInput.value = '#ops:example.org'
    searchInput.focus()

    fireEvent.keyDown(searchInput, { key: 'Tab', shiftKey: true })

    expect(document.activeElement).toBe(joinButton)
  })

  it('submits the live direct room value when input state has not committed yet', async () => {
    let joinBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: '!ops:example.org' } })
        },
      ),
    )
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    const roomInput = (await findByLabelText('Room')) as HTMLInputElement
    roomInput.value = '#ops:example.org'
    fireEvent.keyDown(roomInput, { key: 'Tab' })
    fireEvent.click(await findByRole('button', { name: 'Join' }))

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent('!ops:example.org')}`,
      ),
    )
    expect(joinBody).toEqual({
      room_id_or_alias: '#ops:example.org',
      server_names: [],
    })
  })

  it('joins directly by alias shorthand and routes to the joined room', async () => {
    let joinBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: '!ops:example.org' } })
        },
      ),
    )
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('Room'), {
      target: { value: '#ops' },
    })
    fireEvent.click(await findByRole('button', { name: 'Join' }))

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent('!ops:example.org')}`,
      ),
    )
    expect(joinBody).toEqual({
      room_id_or_alias: '#ops:example.org',
      server_names: [],
    })
  })

  it('disables mobile autocorrect for Matrix identifiers', async () => {
    const { findByLabelText } = renderRoomsIndex()

    const room = (await findByLabelText('Room')) as HTMLInputElement
    const user = (await findByLabelText('User')) as HTMLInputElement
    const invite = (await findByLabelText('Invite')) as HTMLInputElement

    for (const input of [room, user, invite]) {
      expect(input.getAttribute('autocapitalize')).toBe('none')
      expect(input.getAttribute('autocorrect')).toBe('off')
    }
    expect(room.getAttribute('inputmode')).toBe('url')
    expect(user.getAttribute('inputmode')).toBe('email')
    expect(invite.getAttribute('inputmode')).toBe('email')
  })

  it('joins directly by bare alias using the selected account homeserver', async () => {
    let joinBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: '!ops:example.org' } })
        },
      ),
    )
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('Room'), {
      target: { value: 'ops' },
    })
    fireEvent.click(await findByRole('button', { name: 'Join' }))

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent('!ops:example.org')}`,
      ),
    )
    expect(joinBody).toEqual({
      room_id_or_alias: '#ops:example.org',
      server_names: [],
    })
  })

  it('shows a friendly direct-join not-found error', async () => {
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`, () =>
        HttpResponse.json(
          {
            error: {
              code: 'not_found',
              message: 'the server returned an error: [404 / M_NOT_FOUND]',
            },
          },
          { status: 404 },
        ),
      ),
    )
    const { findByLabelText, findByRole, findByText, queryByText } =
      renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('Room'), {
      target: { value: '#missing:example.org' },
    })
    fireEvent.click(await findByRole('button', { name: 'Join' }))

    expect(
      await findByText(
        'Could not find #missing:example.org. Check the room ID or alias. Room names are not join addresses; use a room ID, canonical alias, or Matrix link.',
      ),
    ).toBeTruthy()
    expect(queryByText(/M_NOT_FOUND/)).toBeNull()
  })

  it('creates a private encrypted room and routes to it', async () => {
    const createdRoom = '!created:example.org'
    let createBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms`,
        async ({ request }) => {
          createBody = await request.json()
          return HttpResponse.json({ data: { room_id: createdRoom } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: ACCOUNT_DTO.user_id,
              room_id: createdRoom,
              name: 'Project',
              canonical_alias: null,
              topic: 'Launch',
              last_activity_ts: 0,
            },
          ],
        }),
      ),
    )
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('Name'), {
      target: { value: 'Project' },
    })
    fireEvent.input(await findByLabelText('Topic'), {
      target: { value: 'Launch' },
    })
    fireEvent.input(await findByLabelText('Invite'), {
      target: { value: '@bob, @carol:example.net' },
    })
    fireEvent.click(await findByRole('button', { name: 'Create room' }))

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(createdRoom)}`,
      ),
    )
    expect(createBody).toEqual({
      name: 'Project',
      topic: 'Launch',
      invite: ['@bob:example.org', '@carol:example.net'],
      is_direct: false,
      public: false,
      preset: 'private_chat',
      encrypted: true,
    })
  })

  it('explains that created room names are not joinable aliases', async () => {
    const { findByText } = renderRoomsIndex()

    expect(
      await findByText(
        /Name is display-only, not a joinable alias\. Axon room creation does not support defining aliases yet\./,
      ),
    ).toBeTruthy()
  })

  it('creates a public unencrypted room when selected', async () => {
    let createBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms`,
        async ({ request }) => {
          createBody = await request.json()
          return HttpResponse.json({ data: { room_id: '!public:example.org' } })
        },
      ),
    )
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    fireEvent.change(await findByLabelText('Visibility'), {
      target: { value: 'public' },
    })
    fireEvent.click(await findByLabelText('Encrypt from creation'))
    fireEvent.click(await findByRole('button', { name: 'Create room' }))

    await waitFor(() =>
      expect(createBody).toMatchObject({
        public: true,
        preset: 'public_chat',
        encrypted: false,
      }),
    )
  })

  it('keeps create-room input recoverable when invite validation fails', async () => {
    let created = false
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms`, () => {
        created = true
        return HttpResponse.json({ data: { room_id: '!created:example.org' } })
      }),
    )
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    const name = (await findByLabelText('Name')) as HTMLInputElement
    const invite = (await findByLabelText('Invite')) as HTMLInputElement
    fireEvent.input(name, { target: { value: 'Project' } })
    fireEvent.input(invite, { target: { value: 'not:a:user:id' } })
    fireEvent.click(await findByRole('button', { name: 'Create room' }))

    expect(await findByText(/Enter invitees as Matrix user IDs/)).toBeTruthy()
    expect(name.value).toBe('Project')
    expect(invite.value).toBe('not:a:user:id')
    expect(created).toBe(false)
  })

  it('shows a friendly create-room permission error', async () => {
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms`, () =>
        HttpResponse.json(
          {
            error: {
              code: 'forbidden',
              message: 'the server returned an error: [403 / M_FORBIDDEN]',
            },
          },
          { status: 403 },
        ),
      ),
    )
    const { findByLabelText, findByRole, findByText, queryByText } =
      renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('Name'), {
      target: { value: 'Project' },
    })
    fireEvent.click(await findByRole('button', { name: 'Create room' }))

    expect(
      await findByText(
        'Could not create room. This homeserver does not allow this account to create rooms.',
      ),
    ).toBeTruthy()
    expect(queryByText(/M_FORBIDDEN/)).toBeNull()
  })

  it('looks up a DM profile by normalized localpart and starts the DM', async () => {
    const createdRoom = '!dm:example.org'
    let profileUserId: string | undefined
    let dmBody: unknown = null
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/users/:userId/profile`,
        ({ params }) => {
          profileUserId = params.userId as string
          return HttpResponse.json({
            data: {
              user_id: '@bob:example.org',
              display_name: 'Bob',
              avatar_url: null,
            },
          })
        },
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`,
        async ({ request }) => {
          dmBody = await request.json()
          return HttpResponse.json({ data: { room_id: createdRoom } })
        },
      ),
    )
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('User'), {
      target: { value: 'Bob' },
    })
    fireEvent.click(await findByRole('button', { name: 'Look up profile' }))
    expect(await findByText(/Profile: Bob/)).toBeTruthy()
    fireEvent.click(await findByRole('button', { name: 'Start DM' }))

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(createdRoom)}`,
      ),
    )
    expect(profileUserId).toBe('@bob:example.org')
    expect(dmBody).toEqual({ user_id: '@bob:example.org' })
  })

  it('starts a DM when a full Matrix username is missing @', async () => {
    let dmBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`,
        async ({ request }) => {
          dmBody = await request.json()
          return HttpResponse.json({ data: { room_id: '!dm:example.org' } })
        },
      ),
    )
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('User'), {
      target: { value: 'Bob:Example.NET' },
    })
    fireEvent.click(await findByRole('button', { name: 'Start DM' }))

    await waitFor(() => expect(dmBody).toEqual({ user_id: '@bob:Example.NET' }))
  })

  it('shows a friendly message when profile lookup cannot find a user', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/users/:userId/profile`,
        () =>
          HttpResponse.json(
            {
              error: {
                code: 'not_found',
                message:
                  'the server returned an error: [404 / M_UNKNOWN] No row found (profiles)',
              },
            },
            { status: 404 },
          ),
      ),
    )
    const { findByLabelText, findByRole, findByText, queryByText } =
      renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('User'), {
      target: { value: 'Missing' },
    })
    fireEvent.click(await findByRole('button', { name: 'Look up profile' }))

    expect(
      await findByText(
        'No Matrix profile was found for @missing:example.org. Check the user ID and homeserver, then try again.',
      ),
    ).toBeTruthy()
    expect(queryByText(/No row found/)).toBeNull()
  })

  it('hides raw profile storage miss errors behind the same friendly message', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/users/:userId/profile`,
        () =>
          HttpResponse.json(
            {
              error: {
                code: 'M_UNKNOWN',
                message:
                  'the server returned an error: [404 / M_UNKNOWN] No row found (profiles)',
              },
            },
            { status: 404 },
          ),
      ),
    )
    const { findByLabelText, findByRole, findByText, queryByText } =
      renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('User'), {
      target: { value: '@Missing:example.org' },
    })
    fireEvent.click(await findByRole('button', { name: 'Look up profile' }))

    expect(
      await findByText(
        'No Matrix profile was found for @missing:example.org. Check the user ID and homeserver, then try again.',
      ),
    ).toBeTruthy()
    expect(queryByText(/M_UNKNOWN/)).toBeNull()
  })

  it('keeps DM input recoverable when creation fails', async () => {
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`, () =>
        HttpResponse.json(
          { error: { code: 'forbidden', message: 'blocked' } },
          { status: 403 },
        ),
      ),
    )
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    const user = (await findByLabelText('User')) as HTMLInputElement
    fireEvent.input(user, { target: { value: '@bob:example.org' } })
    fireEvent.click(await findByRole('button', { name: 'Start DM' }))

    expect(await findByText(/Could not start DM/)).toBeTruthy()
    expect(user.value).toBe('@bob:example.org')
  })

  it('searches the account homeserver directory and joins a result', async () => {
    let directoryUrl: string | null = null
    let joinBody: unknown = null
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/directory/public_rooms`,
        ({ request }) => {
          directoryUrl = request.url
          return HttpResponse.json({
            data: {
              chunk: [
                {
                  room_id: '!ops:example.org',
                  canonical_alias: '#ops:example.org',
                  name: 'Ops',
                  topic: 'Operations',
                  avatar_url: null,
                  num_joined_members: 42,
                  world_readable: true,
                  guest_can_join: false,
                  join_rule: 'public',
                  room_type: null,
                },
              ],
              next_batch: null,
              prev_batch: null,
              total_room_count_estimate: 1,
            },
          })
        },
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: '!ops:example.org' } })
        },
      ),
    )
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('Search'), {
      target: { value: 'ops' },
    })
    fireEvent.click(await findByRole('button', { name: 'Search directory' }))
    const card = (await findByText('Ops')).closest('.card')!
    fireEvent.click(
      within(card as HTMLElement).getByRole('button', { name: 'Join' }),
    )

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent('!ops:example.org')}`,
      ),
    )
    expect(directoryUrl).not.toBeNull()
    const directoryQuery = new URL(directoryUrl!).searchParams
    expect(directoryQuery.get('search_term')).toBe('ops')
    expect(directoryQuery.get('server')).toBeNull()
    expect(directoryQuery.get('limit')).toBe('25')
    expect(joinBody).toEqual({
      room_id_or_alias: '#ops:example.org',
      server_names: [],
    })
  })

  it('keeps load-more requests pinned to the submitted directory query', async () => {
    const directoryUrls: string[] = []
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/directory/public_rooms`,
        ({ request }) => {
          directoryUrls.push(request.url)
          const params = new URL(request.url).searchParams
          return HttpResponse.json({
            data: {
              chunk:
                params.get('since') === 'next-ops'
                  ? [
                      {
                        room_id: '!ops-more:example.org',
                        canonical_alias: '#ops-more:example.org',
                        name: 'Ops More',
                        topic: null,
                        avatar_url: null,
                        num_joined_members: 12,
                        world_readable: true,
                        guest_can_join: false,
                        join_rule: 'public',
                        room_type: null,
                      },
                    ]
                  : [
                      {
                        room_id: '!ops:example.org',
                        canonical_alias: '#ops:example.org',
                        name: 'Ops',
                        topic: null,
                        avatar_url: null,
                        num_joined_members: 42,
                        world_readable: true,
                        guest_can_join: false,
                        join_rule: 'public',
                        room_type: null,
                      },
                    ],
              next_batch:
                params.get('since') === 'next-ops' ? null : 'next-ops',
              prev_batch: null,
              total_room_count_estimate: 2,
            },
          })
        },
      ),
    )
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('Search'), {
      target: { value: 'ops' },
    })
    fireEvent.input(await findByLabelText('Directory server'), {
      target: { value: 'matrix.org' },
    })
    fireEvent.click(await findByRole('button', { name: 'Search directory' }))
    await findByText('Ops')

    fireEvent.input(await findByLabelText('Search'), {
      target: { value: 'dev' },
    })
    fireEvent.input(await findByLabelText('Directory server'), {
      target: { value: 'matrixrooms.info' },
    })
    fireEvent.click(await findByRole('button', { name: 'Load more' }))

    await findByText('Ops More')
    expect(directoryUrls).toHaveLength(2)
    const loadMoreQuery = new URL(directoryUrls[1]).searchParams
    expect(loadMoreQuery.get('since')).toBe('next-ops')
    expect(loadMoreQuery.get('search_term')).toBe('ops')
    expect(loadMoreQuery.get('server')).toBe('matrix.org')
  })

  it('clears stale directory results and cursor after a fresh search fails', async () => {
    let requests = 0
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/directory/public_rooms`,
        () => {
          requests += 1
          if (requests > 1) {
            return HttpResponse.json(
              { error: 'directory unavailable' },
              { status: 502 },
            )
          }
          return HttpResponse.json({
            data: {
              chunk: [
                {
                  room_id: '!ops:example.org',
                  canonical_alias: '#ops:example.org',
                  name: 'Ops',
                  topic: null,
                  avatar_url: null,
                  num_joined_members: 42,
                  world_readable: true,
                  guest_can_join: false,
                  join_rule: 'public',
                  room_type: null,
                },
              ],
              next_batch: 'next-ops',
              prev_batch: null,
              total_room_count_estimate: 2,
            },
          })
        },
      ),
    )
    const {
      container,
      findByLabelText,
      findByRole,
      findByText,
      queryByRole,
      queryByText,
    } = renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('Search'), {
      target: { value: 'ops' },
    })
    fireEvent.click(await findByRole('button', { name: 'Search directory' }))
    await findByText('Ops')
    expect(await findByRole('button', { name: 'Load more' })).toBeTruthy()

    fireEvent.input(await findByLabelText('Search'), {
      target: { value: 'broken' },
    })
    fireEvent.click(await findByRole('button', { name: 'Search directory' }))

    await waitFor(() => {
      expect(queryByText('Ops')).toBeNull()
      expect(queryByRole('button', { name: 'Load more' })).toBeNull()
      expect(container.querySelector('.error')?.textContent).not.toBe('')
    })
  })

  it('joins a directory room with an omitted canonical alias by room id and via server', async () => {
    let joinBody: unknown = null
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: ACCOUNT_DTO.user_id,
              room_id: '!other:example.org',
              name: 'Other',
              topic: null,
              last_activity_ts: 0,
            },
          ],
        }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/directory/public_rooms`,
        () =>
          HttpResponse.json({
            data: {
              chunk: [
                {
                  room_id: '!remote:example.org',
                  name: 'Remote',
                  topic: null,
                  avatar_url: null,
                  num_joined_members: 42,
                  world_readable: true,
                  guest_can_join: false,
                  join_rule: 'public',
                  room_type: null,
                },
              ],
              next_batch: null,
              prev_batch: null,
              total_room_count_estimate: 1,
            },
          }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: '!remote:example.org' } })
        },
      ),
    )
    const { findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    fireEvent.click(await findByRole('button', { name: 'Search directory' }))
    const card = (await findByText('Remote')).closest('.card')!
    fireEvent.click(
      within(card as HTMLElement).getByRole('button', { name: 'Join' }),
    )

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent('!remote:example.org')}`,
      ),
    )
    expect(joinBody).toEqual({
      room_id_or_alias: '!remote:example.org',
      server_names: ['example.org'],
    })
  })

  it('opens an already joined directory result without joining again', async () => {
    let joined = false
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: ACCOUNT_DTO.user_id,
              room_id: '!ops:example.org',
              name: 'Ops',
              canonical_alias: '#ops:example.org',
              topic: null,
              last_activity_ts: 0,
            },
          ],
        }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/directory/public_rooms`,
        () =>
          HttpResponse.json({
            data: {
              chunk: [
                {
                  room_id: '!ops:example.org',
                  canonical_alias: '#ops:example.org',
                  name: 'Ops',
                  topic: null,
                  avatar_url: null,
                  num_joined_members: 42,
                  world_readable: true,
                  guest_can_join: false,
                  join_rule: 'public',
                  room_type: null,
                },
              ],
              next_batch: null,
              prev_batch: null,
              total_room_count_estimate: 1,
            },
          }),
      ),
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`, () => {
        joined = true
        return HttpResponse.json({ data: { room_id: '!ops:example.org' } })
      }),
    )
    const { findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    fireEvent.click(await findByRole('button', { name: 'Search directory' }))
    const card = (await findByText('Ops')).closest('.card')!
    fireEvent.click(
      within(card as HTMLElement).getByRole('button', { name: 'Open' }),
    )

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent('!ops:example.org')}`,
      ),
    )
    expect(joined).toBe(false)
  })

  it('searches a chosen directory server and remembers it as a recent', async () => {
    let directoryUrl: string | null = null
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/directory/public_rooms`,
        ({ request }) => {
          directoryUrl = request.url
          return HttpResponse.json({
            data: {
              chunk: [],
              next_batch: null,
              prev_batch: null,
              total_room_count_estimate: 0,
            },
          })
        },
      ),
    )
    const { findByLabelText, findByRole, findByText } = renderRoomsIndex()

    await findByText('example.org')
    fireEvent.input(await findByLabelText('Directory server'), {
      target: { value: 'https://matrixrooms.info/' },
    })
    fireEvent.click(await findByRole('button', { name: 'Search directory' }))

    await waitFor(() => expect(directoryUrl).not.toBeNull())
    expect(new URL(directoryUrl!).searchParams.get('server')).toBe(
      'matrixrooms.info',
    )
    expect(window.localStorage.getItem('axon.publicRoomDirectoryServers')).toBe(
      '["matrixrooms.info"]',
    )
  })

  it('keeps successful directory results when remembering a recent server fails', async () => {
    let directoryUrl: string | null = null
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/directory/public_rooms`,
        ({ request }) => {
          directoryUrl = request.url
          return HttpResponse.json({
            data: {
              chunk: [
                {
                  room_id: '!ops:example.org',
                  canonical_alias: '#ops:example.org',
                  name: 'Ops',
                  topic: null,
                  avatar_url: null,
                  num_joined_members: 42,
                  world_readable: true,
                  guest_can_join: false,
                  join_rule: 'public',
                  room_type: null,
                },
              ],
              next_batch: null,
              prev_batch: null,
              total_room_count_estimate: 1,
            },
          })
        },
      ),
    )
    const storage = memoryStorage()
    storage.setItem = () => {
      throw new Error('quota exceeded')
    }
    const { findByLabelText, findByRole, findByText, queryByText } =
      renderRoomsIndex({ storage })

    await findByText('example.org')
    fireEvent.input(await findByLabelText('Directory server'), {
      target: { value: 'matrix.org' },
    })
    fireEvent.click(await findByRole('button', { name: 'Search directory' }))

    expect(await findByText('Ops')).toBeTruthy()
    expect(queryByText('quota exceeded')).toBeNull()
    expect(directoryUrl).not.toBeNull()
    expect(new URL(directoryUrl!).searchParams.get('server')).toBe('matrix.org')
  })

  it('clears account-scoped discovery and join state when switching accounts', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
        HttpResponse.json({ data: [ACCOUNT_DTO, OTHER_ACCOUNT_DTO] }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/directory/public_rooms`,
        () =>
          HttpResponse.json({
            data: {
              chunk: [
                {
                  room_id: '!ops:example.org',
                  canonical_alias: '#ops:example.org',
                  name: 'Ops',
                  topic: null,
                  avatar_url: null,
                  num_joined_members: 42,
                  world_readable: true,
                  guest_can_join: false,
                  join_rule: 'public',
                  room_type: null,
                },
              ],
              next_batch: 'next-ops',
              prev_batch: null,
              total_room_count_estimate: 2,
            },
          }),
      ),
    )
    const {
      findByLabelText,
      findByRole,
      findByText,
      queryByRole,
      queryByText,
    } = renderRoomsIndex()

    await findByText('example.org')
    const roomInput = (await findByLabelText('Room')) as HTMLInputElement
    fireEvent.input(roomInput, { target: { value: 'not a room' } })
    fireEvent.click(await findByRole('button', { name: 'Join' }))
    await findByText('Enter a room id, alias, Matrix.to link, or matrix: URI.')
    fireEvent.click(await findByRole('button', { name: 'Search directory' }))
    await findByText('Ops')
    expect(await findByRole('button', { name: 'Load more' })).toBeTruthy()

    fireEvent.change(await findByLabelText('Account'), {
      target: { value: OTHER_ACCOUNT },
    })

    await waitFor(() => {
      expect(roomInput.value).toBe('')
      expect(queryByText('Ops')).toBeNull()
      expect(queryByRole('button', { name: 'Load more' })).toBeNull()
      expect(
        queryByText('Enter a room id, alias, Matrix.to link, or matrix: URI.'),
      ).toBeNull()
    })
  })
})

function renderRoomsIndex({
  storage = memoryStorage(),
}: { storage?: Storage } = {}) {
  localStorageMock = storage
  Object.defineProperty(window, 'localStorage', {
    configurable: true,
    value: localStorageMock,
  })
  const services = testServices()
  return render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <RoomsIndex />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
}
