import { signal } from '@preact/signals'
import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { LocationProvider } from 'preact-iso'
import { afterAll, afterEach, beforeAll, expect, it, vi } from 'vitest'
import { ServicesContext } from '../services'
import type { MembersStore } from '../stores/members'
import type { MemberDto } from '../stores/room-list'
import { TEST_BASE_URL, testServices } from '../test/services'
import { UserInfoPanel } from './UserInfoPanel'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ME = '@me:hs'
const ALICE = '@alice:hs'
const EXISTING = '!existing:hs'
const CREATED = '!created:hs'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
})
afterAll(() => server.close())

function member(
  userId: string,
  displayName: string | null,
  extra: Partial<MemberDto> = {},
): MemberDto {
  return {
    user_id: userId,
    membership: 'join',
    display_name: displayName,
    avatar_url: null,
    ...extra,
  }
}

function membersStore(
  rows: readonly MemberDto[],
  loading = false,
): MembersStore {
  return {
    members: signal(new Map(rows.map((row) => [row.user_id, row]))),
    loading: signal(loading),
    error: signal(null),
    displayName: (userId: string) =>
      rows.find((row) => row.user_id === userId)?.display_name?.trim() ||
      userId,
    refresh: vi.fn(),
    inviteUsers: vi.fn(),
    cancelInvite: vi.fn(),
  } as unknown as MembersStore
}

function profileHandler(
  displayName: string | null,
  avatarUrl: string | null = null,
) {
  return http.get(
    `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/users/:userId/profile`,
    () =>
      HttpResponse.json({
        data: {
          user_id: ALICE,
          display_name: displayName,
          avatar_url: avatarUrl,
        },
      }),
  )
}

function deviceHandler(
  devices: readonly Record<string, unknown>[] = [],
  onRequest?: (userId: string | null) => void,
) {
  return http.get(
    `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/devices`,
    ({ request }) => {
      onRequest?.(new URL(request.url).searchParams.get('user_id'))
      return HttpResponse.json({
        data: { user_id: ALICE, devices },
      })
    },
  )
}

function renderPanel({
  userId = ALICE,
  ownUserId = ME,
  rows = [member(ALICE, 'Alice in room')],
  onClose = vi.fn(),
}: {
  userId?: string
  ownUserId?: string | null
  rows?: readonly MemberDto[]
  onClose?: () => void
} = {}) {
  const services = testServices()
  const view = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <UserInfoPanel
          accountId={ACCOUNT}
          userId={userId}
          ownUserId={ownUserId}
          members={membersStore(rows)}
          onClose={onClose}
        />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  return { ...view, onClose }
}

const detail = (label: string): string => {
  const terms = [...document.querySelectorAll('.detail-list dt')]
  const term = terms.find((node) => node.textContent === label)
  return term?.nextElementSibling?.textContent ?? ''
}

it('shows the room profile in the header and a differing global profile as details', async () => {
  let requestedUser: string | null = null
  server.use(
    profileHandler('Alice Global', 'mxc://hs/global'),
    http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:id`, () =>
      HttpResponse.arrayBuffer(new ArrayBuffer(8), {
        headers: { 'content-type': 'image/png' },
      }),
    ),
    deviceHandler(
      [
        {
          device_id: 'ALICEPHONE',
          display_name: 'Phone',
          algorithms: ['m.megolm.v1.aes-sha2'],
          is_verified: true,
          is_cross_signed_by_owner: false,
          local_trust_state: 'unset',
        },
      ],
      (userId) => {
        requestedUser = userId
      },
    ),
  )
  const { findByRole, getByRole, queryByRole } = renderPanel({
    rows: [member(ALICE, 'Alice in room', { avatar_url: 'mxc://hs/room' })],
  })

  expect(document.querySelector('.room-info-identity-name')?.textContent).toBe(
    'Alice in room',
  )
  expect(queryByRole('heading', { name: 'Actions' })).toBeNull()
  const name = document.querySelector('.room-info-identity-name')
  const directMessage = getByRole('button', { name: 'Direct message' })
  const details = getByRole('heading', { name: 'Details' })
  if (name === null) {
    throw new Error('missing user name')
  }
  expect(directMessage.closest('.user-info-intro')).toBe(
    name.closest('.user-info-intro'),
  )
  expect(
    name.compareDocumentPosition(directMessage) &
      Node.DOCUMENT_POSITION_FOLLOWING,
  ).not.toBe(0)
  expect(
    directMessage.compareDocumentPosition(details) &
      Node.DOCUMENT_POSITION_FOLLOWING,
  ).not.toBe(0)
  expect(await findByRole('heading', { name: 'User information' })).toBeTruthy()
  await waitFor(() =>
    expect(detail('Global display name')).toBe('Alice Global'),
  )
  expect(detail('Global avatar')).toBe('mxc://hs/global')
  expect(detail('Room display name')).toBe('Alice in room')
  expect(detail('Room avatar')).toBe('mxc://hs/room')
  expect(detail('Matrix ID')).toBe(ALICE)
  expect(detail('Membership')).toBe('Joined')
  expect(requestedUser).toBe(ALICE)
  await waitFor(() =>
    expect(getByRole('listitem').textContent).toContain('Phone'),
  )
  expect(getByRole('listitem').textContent).toContain('ALICEPHONE')
  expect(getByRole('listitem').textContent).toContain('Verified')
  expect(getByRole('listitem').textContent).toContain(
    'Not cross-signed by owner',
  )
  expect(
    getByRole('button', { name: 'Direct message' }).hasAttribute('disabled'),
  ).toBe(false)
  expect(getByRole('button', { name: 'Ignore' }).hasAttribute('disabled')).toBe(
    true,
  )
  expect(getByRole('button', { name: 'Verify' }).hasAttribute('disabled')).toBe(
    true,
  )
  expect(queryByRole('button', { name: 'Edit display name' })).toBeNull()
  expect(queryByRole('button', { name: 'Call' })).toBeNull()
})

it('omits a global name that matches the room and says when no devices are tracked', async () => {
  server.use(profileHandler('Alice in room', null), deviceHandler())
  const { findByText, queryByText } = renderPanel()
  await findByText('Axon has not tracked any devices for this user.')
  expect(queryByText('Global display name')).toBeNull()
  expect(detail('Room display name')).toBe('Alice in room')
})

it('falls back to the global profile when the user is not in the room', async () => {
  server.use(profileHandler('Alice Global', null), deviceHandler())
  const { queryByRole } = renderPanel({ rows: [] })
  await waitFor(() =>
    expect(
      document.querySelector('.room-info-identity-name')?.textContent,
    ).toBe('Alice Global'),
  )
  expect(detail('Membership')).toBe('Not in this room')
  expect(queryByRole('button', { name: 'Copy Membership' })).toBeNull()
  expect(detail('Display name')).toBe('Alice Global')
  expect(detail('Room display name')).toBe('')
})

it('opens an existing DM and closes, and stays put when creation fails', async () => {
  const posts: string[] = []
  server.use(
    profileHandler('Alice Global'),
    deviceHandler(),
    http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
      HttpResponse.json({
        data: [
          {
            account_id: ACCOUNT,
            account_user_id: ME,
            room_id: EXISTING,
            name: 'Alice',
            is_direct: true,
            last_activity_ts: 1,
            notification_count: 0,
            highlight_count: 0,
          },
        ],
      }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/members`,
      () =>
        HttpResponse.json({
          data: [member(ME, 'Me'), member(ALICE, 'Alice in room')],
        }),
    ),
    http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`, () => {
      posts.push('dm')
      return HttpResponse.json({ data: { room_id: CREATED } })
    }),
  )
  const view = renderPanel()
  fireEvent.click(await view.findByRole('button', { name: 'Direct message' }))
  await waitFor(() => expect(view.onClose).toHaveBeenCalledOnce())
  expect(posts).toEqual([])
  expect(decodeURIComponent(window.location.pathname)).toContain(EXISTING)

  cleanup()
  server.resetHandlers()
  server.use(
    profileHandler('Alice Global'),
    deviceHandler(),
    http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
      HttpResponse.json({ data: [] }),
    ),
    http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`, () =>
      HttpResponse.json(
        { error: { code: 'forbidden', message: 'nope' } },
        { status: 403 },
      ),
    ),
  )
  const failed = renderPanel()
  fireEvent.click(await failed.findByRole('button', { name: 'Direct message' }))
  expect((await failed.findByRole('alert')).textContent).toContain(
    `Could not start DM with ${ALICE}`,
  )
  expect(failed.onClose).not.toHaveBeenCalled()
  expect(
    failed.getByRole('complementary', { name: 'User information' }),
  ).toBeTruthy()
})

it('shows your own information without a direct message, ignore, or verify action', async () => {
  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/users/:userId/profile`,
      () =>
        HttpResponse.json({
          data: { user_id: ME, display_name: 'Me', avatar_url: null },
        }),
    ),
    deviceHandler(),
  )
  const { findByRole, queryByRole } = renderPanel({
    userId: ME,
    ownUserId: ME,
    rows: [member(ME, 'Me')],
  })
  const edit = await findByRole('button', { name: 'Edit display name' })
  expect(edit.hasAttribute('disabled')).toBe(true)
  expect(queryByRole('button', { name: 'Direct message' })).toBeNull()
  expect(queryByRole('button', { name: 'Ignore' })).toBeNull()
  expect(queryByRole('button', { name: 'Verify' })).toBeNull()
  expect(queryByRole('button', { name: 'Call' })).toBeNull()
})

it('reports a profile read failure and still lists devices', async () => {
  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/users/:userId/profile`,
      () =>
        HttpResponse.json(
          { error: { code: 'unavailable', message: 'profile down' } },
          { status: 503 },
        ),
    ),
    deviceHandler(),
  )
  const { findByRole, findByText } = renderPanel()
  expect(document.activeElement?.textContent).toBe('Close')
  expect((await findByRole('alert')).textContent).toBe('profile down')
  await findByText('Axon has not tracked any devices for this user.')
  expect(document.querySelector('.room-info-identity-name')?.textContent).toBe(
    'Alice in room',
  )
})

const PHONE = {
  device_id: 'ALICEPHONE',
  display_name: 'Phone',
  algorithms: ['m.megolm.v1.aes-sha2'],
  is_verified: false,
  is_cross_signed_by_owner: true,
  local_trust_state: 'unset',
}

it('lists devices when the profile read throws', async () => {
  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/users/:userId/profile`,
      () => HttpResponse.error(),
    ),
    deviceHandler([PHONE]),
  )
  const { findByText, findByRole } = renderPanel()
  await findByText('Phone')
  const alert = await findByRole('alert')
  expect(alert.textContent).not.toContain('Phone')
  expect(alert.textContent?.length).toBeGreaterThan(0)
})

it('shows devices while the global profile is still loading', async () => {
  let release!: () => void
  const gate = new Promise<void>((resolve) => {
    release = resolve
  })
  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/users/:userId/profile`,
      async () => {
        await gate
        return HttpResponse.json({
          data: {
            user_id: ALICE,
            display_name: 'Later',
            avatar_url: null,
          },
        })
      },
    ),
    deviceHandler([PHONE]),
  )
  const view = renderPanel()
  await view.findByText('Phone')
  expect(detail('Global profile')).toBe('Loading…')
  release()
  await waitFor(() => expect(detail('Global display name')).toBe('Later'))
})

it('drops a direct-message result after the panel moves to another user', async () => {
  let posted = false
  let release!: (response: Response) => void
  server.use(
    profileHandler('Alice Global'),
    deviceHandler(),
    http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
      HttpResponse.json({ data: [] }),
    ),
    http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`, () => {
      posted = true
      return new Promise<Response>((resolve) => {
        release = resolve
      })
    }),
  )
  const onClose = vi.fn()
  const services = testServices()
  function Panel({ userId }: { userId: string }) {
    return (
      <ServicesContext.Provider value={services}>
        <LocationProvider>
          <UserInfoPanel
            accountId={ACCOUNT}
            userId={userId}
            ownUserId={ME}
            members={membersStore([
              member(ALICE, 'Alice'),
              member('@bob:hs', 'Bob'),
            ])}
            onClose={onClose}
          />
        </LocationProvider>
      </ServicesContext.Provider>
    )
  }
  const view = render(<Panel userId={ALICE} />)
  fireEvent.click(await view.findByRole('button', { name: 'Direct message' }))
  await waitFor(() => expect(posted).toBe(true))
  expect(
    view.getByRole('button', { name: 'Starting…' }).hasAttribute('disabled'),
  ).toBe(true)
  view.rerender(<Panel userId="@bob:hs" />)
  await view.findByRole('button', { name: 'Direct message' })
  release(HttpResponse.json({ data: { room_id: CREATED } }))
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(onClose).not.toHaveBeenCalled()
  expect(decodeURIComponent(window.location.pathname)).not.toContain(CREATED)
  expect(view.queryByRole('alert')).toBeNull()
})

it('copies a display name of None and does not copy membership', async () => {
  server.use(profileHandler('Other'), deviceHandler())
  const view = renderPanel({
    rows: [member(ALICE, 'None', { membership: 'knock' })],
  })
  await waitFor(() => expect(detail('Membership')).toBe('Knocking'))
  expect(detail('Room display name')).toBe('None')
  expect(
    view.getByRole('button', { name: 'Copy Room display name' }),
  ).toBeTruthy()
  expect(view.queryByRole('button', { name: 'Copy Membership' })).toBeNull()
})

it('keeps shown devices and an open avatar when a reconnect refresh fails', async () => {
  server.use(
    profileHandler('Alice Global'),
    http.get(`${TEST_BASE_URL}/v1/media/:account/:server/:id`, () =>
      HttpResponse.arrayBuffer(new ArrayBuffer(8), {
        headers: { 'content-type': 'image/png' },
      }),
    ),
    deviceHandler([PHONE]),
    http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
      HttpResponse.json({ data: [] }),
    ),
    http.get(`${TEST_BASE_URL}/v1/invites`, () =>
      HttpResponse.json({ data: [] }),
    ),
    http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
      HttpResponse.json({ data: [] }),
    ),
  )
  const services = testServices()
  const view = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <UserInfoPanel
          accountId={ACCOUNT}
          userId={ALICE}
          ownUserId={ME}
          members={membersStore([
            member(ALICE, 'Alice in room', { avatar_url: 'mxc://hs/room' }),
          ])}
          onClose={vi.fn()}
        />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  await view.findByText('Phone')
  fireEvent.click(
    view.getByRole('button', {
      name: 'View the Alice in room avatar at full size',
    }),
  )
  expect(
    view.getByRole('dialog', { name: 'Alice in room avatar' }),
  ).toBeTruthy()

  server.use(
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/users/:userId/profile`,
      () =>
        HttpResponse.json(
          { error: { code: 'unavailable', message: 'profile down' } },
          { status: 503 },
        ),
    ),
    http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/devices`, () =>
      HttpResponse.json(
        { error: { code: 'unavailable', message: 'devices down' } },
        { status: 503 },
      ),
    ),
  )
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
  try {
    services.live.start()
    services.sockets[0].emitOpen()
    services.sockets[0].emitClose()
    await vi.advanceTimersByTimeAsync(1000)
    services.sockets.at(-1)!.emitOpen()
    await vi.advanceTimersByTimeAsync(0)
  } finally {
    vi.useRealTimers()
  }
  try {
    await waitFor(() => {
      const alerts = view.getAllByRole('alert').map((node) => node.textContent)
      expect(alerts).toContain('profile down')
      expect(alerts).toContain('devices down')
    })
    expect(view.getByText('Phone')).toBeTruthy()
    expect(
      view.getByRole('dialog', { name: 'Alice in room avatar' }),
    ).toBeTruthy()
  } finally {
    services.live.stop()
  }
})
