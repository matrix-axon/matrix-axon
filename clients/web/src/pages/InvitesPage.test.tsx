import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { LocationProvider, Route, Router } from 'preact-iso'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import type { InviteDto } from '../stores/invites'
import { TEST_BASE_URL, testServices } from '../test/services'
import { InvitesPage } from './InvitesPage'

const ACCOUNT = '11111111-1111-1111-1111-111111111111'

function invite(roomId: string, name: string): InviteDto {
  return {
    account_id: ACCOUNT,
    account_user_id: '@me:hs',
    room_id: roomId,
    name,
    avatar_url: null,
    topic: null,
    canonical_alias: null,
    room_type: null,
    inviter_user_id: '@alice:hs',
    inviter_display_name: 'Alice',
    is_direct: false,
    encrypted: true,
    invited_at: '2026-08-14T12:00:00+00:00',
  }
}

function directInvite(roomId: string): InviteDto {
  return {
    ...invite(roomId, roomId),
    is_direct: true,
  }
}

const server = setupServer(
  http.get(`${TEST_BASE_URL}/v1/invites`, () =>
    HttpResponse.json({ data: [] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/accounts/:accountId/verify`, () =>
    HttpResponse.json({ data: [] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/rooms`, () => HttpResponse.json({ data: [] })),
)
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  history.replaceState(null, '', '/')
})
afterAll(() => server.close())

function renderInbox() {
  history.replaceState(null, '', '/invites')
  const services = testServices()
  const utils = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <Router>
          <Route path="/invites" component={InvitesPage} />
          <Route
            path="/:accountId/rooms/:roomId"
            component={() => <p>Joined room</p>}
          />
        </Router>
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  return { services, ...utils }
}

describe('InvitesPage', () => {
  it('shows the empty state when nothing is pending', async () => {
    const { findByText, queryByRole } = renderInbox()
    expect(await findByText('No pending invites.')).toBeTruthy()
    expect(queryByRole('button', { name: 'Accept all' })).toBeNull()
    expect(queryByRole('button', { name: 'Reject all' })).toBeNull()
  })

  it('lists a pending invite with accept and reject', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/invites`, () =>
        HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] }),
      ),
    )
    const { findByText, getByRole } = renderInbox()
    expect(await findByText('Alpha')).toBeTruthy()
    expect(await findByText(/Invited by Alice \(@alice:hs\)/)).toBeTruthy()
    expect(getByRole('button', { name: 'Accept' })).toBeTruthy()
    expect(getByRole('button', { name: 'Reject' })).toBeTruthy()
    expect(getByRole('button', { name: 'Accept all' })).toBeTruthy()
    expect(getByRole('button', { name: 'Reject all' })).toBeTruthy()
  })

  it('uses a human-readable title when an unnamed DM has a room-ID fallback name', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/invites`, () =>
        HttpResponse.json({ data: [directInvite('!opaque-room-id:hs')] }),
      ),
    )
    const { findByText, queryByText } = renderInbox()

    expect(await findByText('Direct message')).toBeTruthy()
    expect((await findByText(/Invited by Alice/)).textContent).toBe(
      'Invited by Alice (@alice:hs) · Encrypted',
    )
    expect(queryByText('!opaque-room-id:hs')).toBeNull()
  })

  it('falls through an opaque non-DM name to the canonical alias', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/invites`, () =>
        HttpResponse.json({
          data: [
            {
              ...invite('!opaque-group-id:hs', '!opaque-group-id:hs'),
              canonical_alias: '#ops:hs',
            },
          ],
        }),
      ),
    )
    const { findByText, queryByText } = renderInbox()

    expect(await findByText('#ops:hs')).toBeTruthy()
    expect(queryByText('!opaque-group-id:hs')).toBeNull()
  })

  it('reject posts leave and removes the row', async () => {
    const left: string[] = []
    server.use(
      http.get(`${TEST_BASE_URL}/v1/invites`, () =>
        HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        ({ params }) => {
          left.push(String(params.roomId))
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { findByText, findByRole, queryByText } = renderInbox()
    fireEvent.click(await findByRole('button', { name: 'Reject' }))
    await waitFor(() => expect(left).toEqual(['!a:hs']))
    expect(await findByText('No pending invites.')).toBeTruthy()
    expect(queryByText('Alpha')).toBeNull()
  })

  it('accept posts join and routes into the room', async () => {
    const joined: string[] = []
    server.use(
      http.get(`${TEST_BASE_URL}/v1/invites`, () =>
        HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          const body = (await request.json()) as { room_id_or_alias: string }
          joined.push(body.room_id_or_alias)
          return HttpResponse.json({ data: { room_id: body.room_id_or_alias } })
        },
      ),
    )
    const { findByRole, findByText } = renderInbox()
    fireEvent.click(await findByRole('button', { name: 'Accept' }))
    await waitFor(() => expect(joined).toEqual(['!a:hs']))
    expect(await findByText('Joined room')).toBeTruthy()
    expect(decodeURIComponent(window.location.pathname)).toBe(
      `/${ACCOUNT}/rooms/!a:hs`,
    )
  })

  it('disables every row while one invite is mutating', async () => {
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(`${TEST_BASE_URL}/v1/invites`, () =>
        HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        async () => {
          await held
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { findByText, findAllByRole } = renderInbox()
    expect(await findByText('Alpha')).toBeTruthy()
    const rejects = await findAllByRole('button', { name: 'Reject' })
    fireEvent.click(rejects[0])
    await waitFor(() => {
      const buttons = document.querySelectorAll(
        '.invite-actions button, .invites-bulk button',
      )
      expect(
        [...buttons].every((button) => (button as HTMLButtonElement).disabled),
      ).toBe(true)
    })
    release?.()
    expect(await findByText('Beta')).toBeTruthy()
  })

  it('shows a bulk failure banner when reject all partly fails', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/invites`, () =>
        HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        ({ params }) => {
          if (params.roomId === '!a:hs') {
            return HttpResponse.json(
              { error: { code: 'not_found', message: 'room not found' } },
              { status: 404 },
            )
          }
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { findByRole } = renderInbox()
    fireEvent.click(await findByRole('button', { name: 'Reject all' }))
    const alert = await findByRole('alert')
    expect(alert.textContent).toMatch(/1 succeeded, 1 failed/)
    expect(alert.textContent).toMatch(/Alpha: room not found/)
  })

  it('clears a stale bulk banner once the row is rejected on its own', async () => {
    // The banner describes a run that is over. Leaving it up while the user
    // clears the remaining rows one at a time keeps claiming a failure that
    // no longer applies.
    let failLeave = true
    server.use(
      http.get(`${TEST_BASE_URL}/v1/invites`, () =>
        HttpResponse.json({
          data: failLeave ? [invite('!a:hs', 'Alpha')] : [],
        }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        () =>
          failLeave
            ? HttpResponse.json(
                { error: { code: 'not_found', message: 'room not found' } },
                { status: 404 },
              )
            : HttpResponse.json({ data: {} }),
      ),
    )
    const { findByRole, queryByRole } = renderInbox()
    fireEvent.click(await findByRole('button', { name: 'Reject all' }))
    expect((await findByRole('alert')).textContent).toMatch(/0 succeeded/)

    failLeave = false
    fireEvent.click(await findByRole('button', { name: 'Reject' }))
    await waitFor(() => {
      expect(queryByRole('alert')).toBeNull()
    })
  })

  it('keeps other bulk-failure lines after a single-row action', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/invites`, () =>
        HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        ({ params }) => {
          if (params.roomId === '!a:hs') {
            return HttpResponse.json(
              { error: { code: 'not_found', message: 'room not found' } },
              { status: 404 },
            )
          }
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { findByRole, findByText, findAllByRole, queryByText, services } =
      renderInbox()
    fireEvent.click(await findByRole('button', { name: 'Reject all' }))
    expect((await findByRole('alert')).textContent).toMatch(
      /Alpha: room not found/,
    )

    services.invites.noteAdded(invite('!c:hs', 'Gamma'))
    expect(await findByText('Gamma')).toBeTruthy()
    const rejects = await findAllByRole('button', { name: 'Reject' })
    // `noteAdded` prepends, so Gamma's Reject is first.
    fireEvent.click(rejects[0])
    await waitFor(() => {
      expect(queryByText('Gamma')).toBeNull()
    })
    const alert = await findByRole('alert')
    expect(alert.textContent).toMatch(/Alpha: room not found/)
  })

  it('drops a bulk-failure line once that invite is gone from the list', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/invites`, () =>
        HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        ({ params }) => {
          if (params.roomId === '!a:hs') {
            return HttpResponse.json(
              { error: { code: 'not_found', message: 'room not found' } },
              { status: 404 },
            )
          }
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { findByRole, queryByRole, services } = renderInbox()
    fireEvent.click(await findByRole('button', { name: 'Reject all' }))
    expect((await findByRole('alert')).textContent).toMatch(
      /Alpha: room not found/,
    )

    services.invites.noteRemoved(ACCOUNT, '!a:hs')
    await waitFor(() => {
      expect(queryByRole('alert')).toBeNull()
    })
  })

  it('refreshes the room list once per bulk accept', async () => {
    let roomGets = 0
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () => {
        roomGets += 1
        return HttpResponse.json({ data: [] })
      }),
      http.get(`${TEST_BASE_URL}/v1/invites`, () =>
        HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] }),
      ),
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`, () =>
        HttpResponse.json({ data: { room_id: '!a:hs' } }),
      ),
    )
    const { findByRole } = renderInbox()
    const before = roomGets
    fireEvent.click(await findByRole('button', { name: 'Accept all' }))
    await waitFor(() => {
      expect(roomGets).toBeGreaterThan(before)
    })
    // Let any second refresh land before asserting there wasn't one.
    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(roomGets - before).toBe(1)
  })
})
