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
  expect(getByRole('listitem').textContent).toContain('Phone')
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
  renderPanel({ rows: [] })
  await waitFor(() =>
    expect(
      document.querySelector('.room-info-identity-name')?.textContent,
    ).toBe('Alice Global'),
  )
  expect(detail('Membership')).toBe('Not in this room')
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
  expect((await findByRole('alert')).textContent).toBe('profile down')
  await findByText('Axon has not tracked any devices for this user.')
  expect(document.querySelector('.room-info-identity-name')?.textContent).toBe(
    'Alice in room',
  )
})
