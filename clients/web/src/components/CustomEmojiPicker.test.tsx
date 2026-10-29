import { signal } from '@preact/signals'
import {
  act,
  cleanup,
  fireEvent,
  render,
  waitFor,
} from '@testing-library/preact'
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { options } from 'preact'
import { afterAll, afterEach, beforeAll, expect, it, vi } from 'vitest'
import { ServicesContext } from '../services'
import { testServices, TEST_BASE_URL } from '../test/services'
import { ImagePackPicker } from './CustomEmojiPicker'

const PATH = `${TEST_BASE_URL}/v1/accounts/:account/rooms/:room/emoji-packs`
const server = setupServer()
const previousScheduler = options.requestAnimationFrame
beforeAll(() => {
  options.requestAnimationFrame = queueMicrotask
  server.listen({ onUnhandledRequest: 'error' })
})
afterEach(() => {
  cleanup()
  server.resetHandlers()
  vi.restoreAllMocks()
})
afterAll(() => {
  server.close()
  options.requestAnimationFrame = previousScheduler
})

function data(name = 'wave') {
  return {
    data: {
      partial: false,
      packs: [
        {
          room_id: null,
          event_type: 'im.ponies.user_emotes',
          state_key: null,
          display_name: null,
          images: [{ shortcode: name, url: `mxc://hs/personal-${name}` }],
        },
        {
          room_id: '!room:hs',
          event_type: 'im.ponies.room_emotes',
          state_key: 'Testing',
          display_name: 'Testing',
          images: [{ shortcode: name, url: `mxc://hs/room-${name}` }],
        },
      ],
    },
  }
}
function setup(signedIn = true) {
  const services = testServices({
    token: signedIn ? 'picker-token' : null,
  })
  const reconnects = signal(0)
  services.live = { ...services.live, reconnects }
  // Test media ownership without relying on jsdom decoding image bytes.
  const release = vi.fn()
  vi.spyOn(services.media, 'acquire').mockResolvedValue({
    release,
    result: {
      ok: true,
      url: 'blob:preview',
      format: { kind: 'png', mime: 'image/png' },
    },
  } as never)
  const onReact = vi.fn()
  const view = render(
    <ServicesContext.Provider value={services}>
      <ImagePackPicker
        accountId="account"
        roomId="!room:hs"
        onSelect={onReact}
      />
    </ServicesContext.Provider>,
  )
  return { ...view, services, reconnects, release, onReact }
}

it('groups packs, filters names, sends the selected identity and releases previews', async () => {
  server.use(
    http.get(PATH, ({ request }) => {
      expect(request.headers.get('authorization')).toBe('Bearer picker-token')
      return HttpResponse.json(data())
    }),
  )
  const view = setup()
  await waitFor(() =>
    expect(
      view.getAllByRole('button', { name: 'React with :wave:' }),
    ).toHaveLength(2),
  )
  expect(view.getByRole('heading', { name: 'Personal emoji' })).toBeTruthy()
  expect(view.getByRole('heading', { name: 'Testing' })).toBeTruthy()
  fireEvent.click(view.getAllByRole('button', { name: 'React with :wave:' })[1])
  expect(view.onReact).toHaveBeenCalledWith(
    expect.objectContaining({ url: 'mxc://hs/room-wave', shortcode: 'wave' }),
  )
  fireEvent.input(view.getByRole('searchbox'), { target: { value: 'absent' } })
  expect(view.getByText('No matching custom emoji.')).toBeTruthy()
  await waitFor(() => expect(view.release).toHaveBeenCalled())
  view.unmount()
})

it('offers retry on older servers and refetches after reconnect', async () => {
  let calls = 0
  server.use(
    http.get(PATH, () =>
      ++calls === 1
        ? new HttpResponse(null, { status: 404 })
        : HttpResponse.json(data(`wave${calls}`)),
    ),
  )
  const view = setup()
  await waitFor(() =>
    expect(
      view.getByText('Custom emoji are unavailable for this room or server.'),
    ).toBeTruthy(),
  )
  fireEvent.click(view.getByRole('button', { name: 'Retry' }))
  await waitFor(() =>
    expect(
      view.getAllByRole('button', { name: 'React with :wave2:' }),
    ).toHaveLength(2),
  )
  await act(() => {
    view.reconnects.value++
  })
  await waitFor(() =>
    expect(
      view.getAllByRole('button', { name: 'React with :wave3:' }),
    ).toHaveLength(2),
  )
  expect(view.queryByRole('button', { name: 'React with :wave2:' })).toBeNull()
})

it('discards a pending response when the account changes', async () => {
  let finish!: () => void
  const pending = new Promise<void>((resolve) => {
    finish = resolve
  })
  let seen = false
  server.use(
    http.get(PATH, async ({ params }) => {
      if (params.account === 'account') {
        seen = true
        await pending
        return HttpResponse.json(data('old'))
      }
      return HttpResponse.json(data('fresh'))
    }),
  )
  const view = setup()
  await waitFor(() => expect(seen).toBe(true))
  view.rerender(
    <ServicesContext.Provider value={view.services}>
      <ImagePackPicker
        accountId="other"
        roomId="!room:hs"
        onSelect={view.onReact}
      />
    </ServicesContext.Provider>,
  )
  await waitFor(() =>
    expect(
      view.getAllByRole('button', { name: 'React with :fresh:' }),
    ).toHaveLength(2),
  )
  await act(async () => {
    finish()
    await pending
  })
  expect(view.queryByRole('button', { name: 'React with :old:' })).toBeNull()
  expect(
    view.getAllByRole('button', { name: 'React with :fresh:' }),
  ).toHaveLength(2)
})

it('shows a signed-out message instead of indefinite loading', async () => {
  const read = vi.fn(() => HttpResponse.json(data()))
  server.use(http.get(PATH, read))
  const view = setup(false)
  await waitFor(() =>
    expect(view.getByRole('status').textContent).toContain('Sign in'),
  )
  expect(view.queryByText('Loading custom emoji…')).toBeNull()
  expect(
    view.getByRole('button', { name: 'Refresh' }).hasAttribute('disabled'),
  ).toBe(true)
  expect(view.queryByRole('button', { name: 'Retry' })).toBeNull()
  expect(read).not.toHaveBeenCalled()
})

it('clears visible packs and shows a signed-out message on session expiry', async () => {
  server.use(http.get(PATH, () => HttpResponse.json(data())))
  const view = setup()
  await view.findAllByRole('button', { name: 'React with :wave:' })
  await act(async () => {
    await view.services.auth.clearToken()
  })
  await waitFor(() =>
    expect(view.getByRole('status').textContent).toContain('Sign in'),
  )
  expect(view.queryByRole('button', { name: 'React with :wave:' })).toBeNull()
})

it('normalizes names and omits non-token names only for inline insertion', async () => {
  const response = data(' party ')
  response.data.packs[0].images.push({
    shortcode: 'a:b',
    url: 'mxc://hs/colon',
  })
  server.use(http.get(PATH, () => HttpResponse.json(response)))
  const view = setup()
  await waitFor(() =>
    expect(view.getByRole('button', { name: 'React with :a:b:' })).toBeTruthy(),
  )
  view.rerender(
    <ServicesContext.Provider value={view.services}>
      <ImagePackPicker
        accountId="account"
        roomId="!room:hs"
        action="insert"
        onSelect={view.onReact}
      />
    </ServicesContext.Provider>,
  )
  await waitFor(() =>
    expect(
      view.getAllByRole('button', { name: 'Insert :party:' }),
    ).toHaveLength(2),
  )
  expect(view.queryByRole('button', { name: 'Insert :a:b:' })).toBeNull()
  expect(
    view.getByText('Some emoji names cannot be inserted inline.'),
  ).toBeTruthy()
  fireEvent.click(view.getAllByRole('button', { name: 'Insert :party:' })[0])
  expect(view.onReact).toHaveBeenCalledWith(
    expect.objectContaining({ shortcode: 'party' }),
  )
})
