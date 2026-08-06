import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { App } from '../app'
import type { components } from '../api/schema'
import { TEST_BASE_URL, testServices } from '../test/services'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!ops:hs'

function account(): components['schemas']['AccountDto'] {
  return {
    account_id: ACCOUNT,
    user_id: '@alice:hs',
    homeserver_url: 'https://hs',
    state: 'active',
    sync_state: 'ready',
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
}

function room(): components['schemas']['RoomDto'] {
  return {
    account_id: ACCOUNT,
    account_user_id: '@alice:hs',
    room_id: ROOM,
    name: 'Ops Team',
    last_activity_ts: 1000,
    highlight_count: 0,
    notification_count: 0,
  }
}

function hit(id: string, body: string) {
  return {
    score: 1,
    event: {
      account_id: ACCOUNT,
      event_id: id,
      room_id: ROOM,
      sender: '@bob:hs',
      origin_ts: 1_700_000_000_000,
      arrival_order: 1_700_000_000_000,
      type: 'm.room.message',
      body,
      redacted: false,
      edited: false,
      edit_count: 0,
    },
  }
}

const server = setupServer(
  http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
    HttpResponse.json({ data: [account()] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
    HttpResponse.json({ data: [room()] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/status`, () =>
    HttpResponse.json({
      data: { backfill: { paused: false, free_bytes: 0, accounts: [] } },
    }),
  ),
)
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  history.replaceState(null, '', '/')
})
afterAll(() => server.close())

function searchHandler(
  pages: Record<
    string,
    { results: unknown[]; total: number; next_cursor: string | null }
  >,
  seen?: URLSearchParams[],
) {
  return http.get(`${TEST_BASE_URL}/v1/search`, ({ request }) => {
    const params = new URL(request.url).searchParams
    seen?.push(params)
    return HttpResponse.json({ data: pages[params.get('cursor') ?? ''] })
  })
}

describe('SearchOverlay', () => {
  it('opens from ?search= with the hint, and Escape strips the param', async () => {
    history.replaceState(null, '', '/?search=')
    const { getByRole, queryByRole } = render(<App services={testServices()} />)
    const dialog = getByRole('dialog', { name: 'Search messages' })
    expect(dialog.textContent).toContain('Type to search')

    fireEvent.keyDown(document, { key: 'Escape' })
    await waitFor(() => {
      expect(queryByRole('dialog', { name: 'Search messages' })).toBeNull()
    })
    expect(window.location.search).toBe('')
  })

  it('opens with / and the topbar button', async () => {
    const { getByRole, queryByRole } = render(<App services={testServices()} />)
    expect(queryByRole('dialog', { name: 'Search messages' })).toBeNull()

    fireEvent.keyDown(document, { key: '/' })
    await waitFor(() => {
      expect(getByRole('dialog', { name: 'Search messages' })).toBeTruthy()
    })
    expect(window.location.search).toBe('?search=')
  })

  it('runs the URL query and renders highlighted, linked hits', async () => {
    const seen: URLSearchParams[] = []
    server.use(
      searchHandler(
        {
          '': {
            results: [hit('$1', 'the deploy failed again')],
            total: 1,
            next_cursor: null,
          },
        },
        seen,
      ),
    )
    history.replaceState(
      null,
      '',
      `/?search=${encodeURIComponent('all:true deploy')}`,
    )
    const { getByRole, container } = render(<App services={testServices()} />)

    await waitFor(() => {
      expect(getByRole('dialog').textContent).toContain('1 result')
    })
    expect(seen[0].get('q')).toBe('deploy')
    expect(seen[0].has('account_id')).toBe(false)

    const mark = container.querySelector('.search-hit-body mark')
    expect(mark?.textContent).toBe('deploy')
    const link = container.querySelector<HTMLAnchorElement>('a.search-hit')
    expect(link?.getAttribute('href')).toBe(
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%241`,
    )
    // Room context comes from the rooms store (loaded async), sender from
    // the MXID.
    await waitFor(() => {
      expect(container.querySelector('a.search-hit')?.textContent).toContain(
        'Ops Team',
      )
    })
    expect(link?.textContent).toContain('@bob')
  })

  it('routes a thread hit to its thread-local event deep link', async () => {
    server.use(
      searchHandler({
        '': {
          results: [
            {
              ...hit('$reply', 'the threaded deploy failed'),
              event: {
                ...hit('$reply', 'the threaded deploy failed').event,
                relates_to: { rel_type: 'm.thread', event_id: '$root' },
              },
            },
          ],
          total: 1,
          next_cursor: null,
        },
      }),
    )
    history.replaceState(null, '', `/?search=${encodeURIComponent('deploy')}`)
    const { container, getByRole } = render(<App services={testServices()} />)

    await waitFor(() => {
      expect(getByRole('dialog').textContent).toContain('1 result')
    })

    expect(
      container
        .querySelector<HTMLAnchorElement>('a.search-hit')
        ?.getAttribute('href'),
    ).toBe(
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root&event=%24reply`,
    )
  })

  it('submits the typed input merged over the URL chips', async () => {
    const seen: URLSearchParams[] = []
    server.use(
      searchHandler(
        {
          '': { results: [], total: 0, next_cursor: null },
        },
        seen,
      ),
    )
    history.replaceState(null, '', '/?search=')
    const { getByRole, getByLabelText } = render(
      <App services={testServices()} />,
    )

    const input = getByLabelText('Search query')
    fireEvent.input(input, { target: { value: 'sender:bob deploy' } })
    fireEvent.click(getByRole('button', { name: 'Search' }))

    await waitFor(() => {
      expect(seen).toHaveLength(1)
    })
    expect(seen[0].get('q')).toBe('deploy')
    expect(seen[0].get('sender')).toBe('bob')
    // The token became a chip and the URL carries the canonical form.
    expect(getByRole('dialog').textContent).toContain('sender: bob')
    expect(decodeURIComponent(window.location.search)).toContain('sender:bob')
    await waitFor(() => {
      expect(getByRole('dialog').textContent).toContain('No matches')
    })
  })

  it('submits a field-only sender search as a sender parameter, not q text', async () => {
    const seen: URLSearchParams[] = []
    server.use(
      searchHandler({ '': { results: [], total: 0, next_cursor: null } }, seen),
    )
    history.replaceState(null, '', '/?search=')
    const { getByRole, getByLabelText } = render(
      <App services={testServices()} />,
    )

    const input = getByLabelText('Search query')
    fireEvent.input(input, { target: { value: 'sender:bob' } })
    fireEvent.click(getByRole('button', { name: 'Search' }))

    await waitFor(() => {
      expect(seen).toHaveLength(1)
    })
    expect(seen[0].has('q')).toBe(false)
    expect(seen[0].get('sender')).toBe('bob')
    expect(decodeURIComponent(window.location.search)).toContain('sender:bob')
  })

  it('removing a chip re-runs without that filter', async () => {
    const seen: URLSearchParams[] = []
    server.use(
      searchHandler({ '': { results: [], total: 0, next_cursor: null } }, seen),
    )
    history.replaceState(
      null,
      '',
      `/?search=${encodeURIComponent('all:true sender:bob deploy')}`,
    )
    const { getByRole } = render(<App services={testServices()} />)
    await waitFor(() => expect(seen).toHaveLength(1))

    fireEvent.click(getByRole('button', { name: 'Remove filter: sender: bob' }))
    await waitFor(() => expect(seen).toHaveLength(2))
    expect(seen[1].has('sender')).toBe(false)
    expect(seen[1].get('q')).toBe('deploy')
  })

  it('pages with the Load more button', async () => {
    server.use(
      searchHandler({
        '': {
          results: [hit('$1', 'deploy one')],
          total: 2,
          next_cursor: 'c1',
        },
        c1: {
          results: [hit('$2', 'deploy two')],
          total: 2,
          next_cursor: null,
        },
      }),
    )
    history.replaceState(
      null,
      '',
      `/?search=${encodeURIComponent('all:true deploy')}`,
    )
    const { getByRole, container } = render(<App services={testServices()} />)

    await waitFor(() => {
      expect(getByRole('button', { name: 'Load more results' })).toBeTruthy()
    })
    fireEvent.click(getByRole('button', { name: 'Load more results' }))
    await waitFor(() => {
      expect(container.querySelectorAll('a.search-hit')).toHaveLength(2)
    })
    expect(getByRole('dialog').textContent).not.toContain('Load more results')
  })

  it('renders the unavailable state on a 503', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/search`, () =>
        HttpResponse.json(
          { error: { code: 'unavailable', message: 'disabled' } },
          { status: 503 },
        ),
      ),
    )
    history.replaceState(
      null,
      '',
      `/?search=${encodeURIComponent('all:true x')}`,
    )
    const { getByRole } = render(<App services={testServices()} />)
    await waitFor(() => {
      expect(getByRole('dialog').textContent).toContain(
        'Search is not enabled on this server.',
      )
    })
  })

  it('blocks the submit on an unresolvable token instead of dropping it', async () => {
    // No /v1/search handler: a request here would trip the unhandled-request
    // gate. Typing a room: that resolves to nothing (the live-check bug: the
    // filter silently vanished and the search ran unscoped) must surface an
    // error and keep the box's text for correction.
    history.replaceState(null, '', '/?search=')
    const { getByRole, getByLabelText } = render(
      <App services={testServices()} />,
    )
    const input = getByLabelText('Search query') as HTMLInputElement
    fireEvent.input(input, { target: { value: 'room:nowhere deploy' } })
    fireEvent.click(getByRole('button', { name: 'Search' }))

    await waitFor(() => {
      expect(getByRole('alert').textContent).toContain('nowhere')
    })
    expect(input.value).toBe('room:nowhere deploy')
    expect(window.location.search).toBe('?search=')
  })

  it('shows a parse error instead of searching', async () => {
    // No search handler registered: a request would fail the unhandled-
    // request gate, which is the assertion.
    history.replaceState(
      null,
      '',
      `/?search=${encodeURIComponent('all:true after:nonsense x')}`,
    )
    const { getByRole } = render(<App services={testServices()} />)
    await waitFor(() => {
      expect(getByRole('alert').textContent).toContain('unrecognized date')
    })
  })
})
